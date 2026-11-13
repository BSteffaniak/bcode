//! Store tests against temporary repositories and state directories.

use super::db::*;
use super::*;

#[test]
fn materialization_reports_empty_diff_without_failing_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let workspace = test_workspace(
        &repo,
        vec![test_source(
            "empty-unstaged",
            ReviewSourceKind::WorkingTreeUnstaged,
        )],
    );

    let response = materialize_review_workspace(&repo, workspace)
        .expect("empty diff is a successful materialization");

    assert_eq!(
        response.surfaces,
        [] as [bcode_code_review_models::ReviewSurface; 0]
    );
}

#[test]
fn multi_source_materialization_preserves_success_when_one_source_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let workspace = test_workspace(
        &repo,
        vec![
            test_source("valid", ReviewSourceKind::LastCommit),
            test_source(
                "invalid",
                ReviewSourceKind::BranchCompare {
                    base_branch: "does-not-exist".to_string(),
                    head_branch: "HEAD".to_string(),
                    merge_base: true,
                },
            ),
        ],
    );

    let response =
        materialize_review_workspace(&repo, workspace).expect("partial workspace materialization");

    assert!(
        response
            .surfaces
            .iter()
            .any(|surface| surface.source_id == "valid")
    );
}

fn initialized_test_repository(repo: &Path) -> PathBuf {
    git_test(repo, &["init", "--quiet"]);
    git_test(repo, &["config", "user.email", "review@example.test"]);
    git_test(repo, &["config", "user.name", "Review Test"]);
    std::fs::write(repo.join("file.txt"), "base\n").expect("write base");
    git_test(repo, &["add", "file.txt"]);
    git_test(repo, &["commit", "--quiet", "-m", "base"]);
    std::fs::write(repo.join("file.txt"), "base\nhead\n").expect("write head");
    git_test(repo, &["add", "file.txt"]);
    git_test(repo, &["commit", "--quiet", "-m", "head"]);
    repo.to_path_buf()
}

fn test_workspace(repo: &Path, sources: Vec<ReviewSource>) -> ReviewWorkspace {
    ReviewWorkspace {
        id: "workspace".to_string(),
        title: "Test review".to_string(),
        repo_root: repo.to_path_buf(),
        sources,
        created_at_ms: None,
        updated_at_ms: None,
        viewed_files: BTreeSet::new(),
        archived_at_ms: None,
        viewed_marks: Vec::new(),
        viewed_threads: Vec::new(),
        reviewed_revisions: std::collections::BTreeMap::new(),
        presentation: None,
        stack_snapshots: Vec::new(),
    }
}

fn test_source(id: &str, kind: ReviewSourceKind) -> ReviewSource {
    ReviewSource {
        id: id.to_string(),
        label: id.to_string(),
        kind,
        included: true,
    }
}

#[test]
fn all_common_git_targets_materialize_expected_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    git_test(repo, &["init", "--quiet"]);
    git_test(repo, &["config", "user.email", "review@example.test"]);
    git_test(repo, &["config", "user.name", "Review Test"]);
    std::fs::write(repo.join("file.txt"), "base\n").expect("write base");
    git_test(repo, &["add", "file.txt"]);
    git_test(repo, &["commit", "--quiet", "-m", "base"]);
    let base = git_test_output(repo, &["rev-parse", "HEAD"]);
    git_test(repo, &["branch", "base-branch"]);
    std::fs::write(repo.join("file.txt"), "base\ncommit\n").expect("write commit");
    git_test(repo, &["add", "file.txt"]);
    git_test(repo, &["commit", "--quiet", "-m", "head"]);
    let head = git_test_output(repo, &["rev-parse", "HEAD"]);
    git_test(repo, &["branch", "head-branch"]);
    std::fs::write(repo.join("file.txt"), "base\ncommit\nstaged\n").expect("write staged");
    git_test(repo, &["add", "file.txt"]);
    std::fs::write(repo.join("file.txt"), "base\ncommit\nstaged\nunstaged\n")
        .expect("write unstaged");

    let targets = [
        ReviewTarget::WorkingTreeUnstaged,
        ReviewTarget::IndexStaged,
        ReviewTarget::WorkingTreeAndIndex,
        ReviewTarget::LastCommit,
        ReviewTarget::CommitRange {
            base,
            head,
            merge_base: false,
        },
        ReviewTarget::BranchCompare {
            base_branch: "base-branch".to_string(),
            head_branch: "head-branch".to_string(),
            merge_base: true,
        },
    ];
    for target in targets {
        let summary = create_review_summary(&CreateReviewRequest {
            repo_path: repo.to_path_buf(),
            target,
        })
        .expect("materialize target");
        assert_ne!(
            summary.files,
            [] as [bcode_code_review_models::ReviewFile; 0]
        );
    }
}

fn git_test(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("run git")
            .success()
    );
}

fn git_test_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_string()
}

#[test]
fn external_import_persistence_round_trips_and_replaces_snapshot_atomically() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git init")
            .success()
    );
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let thread = bcode_code_review_models::ExternalReviewThread {
        identity: bcode_code_review_models::ExternalReviewIdentity {
            provider_id: "github".to_string(),
            external_id: "thread:1".to_string(),
            url: None,
        },
        state: bcode_code_review_models::ExternalReviewThreadState::Open,
        mapping: bcode_code_review_models::ExternalReviewAnchorMapping {
            status: bcode_code_review_models::ExternalAnchorMappingStatus::Unmappable,
            anchor: None,
            message: Some("outdated".to_string()),
        },
        comments: Vec::new(),
    };
    let saved = save_external_import_for_request(
        SaveExternalReviewImportRequest {
            repo_path: repo.clone(),
            workspace_id: "workspace-1".to_string(),
            import: bcode_code_review_models::ExternalReviewImportResponse {
                importer_id: "github_pr_review".to_string(),
                threads: vec![thread.clone()],
                warnings: vec!["first warning".to_string()],
            },
        },
        &config,
    )
    .expect("save import");
    assert_eq!(saved.record.threads, vec![thread]);

    let replacement = save_external_import_for_request(
        SaveExternalReviewImportRequest {
            repo_path: repo.clone(),
            workspace_id: "workspace-1".to_string(),
            import: bcode_code_review_models::ExternalReviewImportResponse {
                importer_id: "github_pr_review".to_string(),
                threads: Vec::new(),
                warnings: vec!["replacement".to_string()],
            },
        },
        &config,
    )
    .expect("replace import");
    let listed = list_external_imports_for_request(
        &ListExternalReviewImportsRequest {
            repo_path: repo,
            workspace_id: "workspace-1".to_string(),
        },
        &config,
    )
    .expect("list imports");

    assert_eq!(listed.records, vec![replacement.record]);
    assert_eq!(
        listed.records[0].threads,
        [] as [bcode_code_review_models::ExternalReviewThread; 0]
    );
    assert_eq!(listed.records[0].warnings, vec!["replacement"]);
}

#[test]
fn publish_record_thread_membership_round_trips() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git init")
            .success()
    );
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let workspace = ReviewWorkspace {
        id: "workspace-publish".to_string(),
        title: "Publish".to_string(),
        repo_root: repo.clone(),
        sources: Vec::new(),
        created_at_ms: None,
        updated_at_ms: None,
        viewed_files: BTreeSet::new(),
        archived_at_ms: None,
        viewed_marks: Vec::new(),
        viewed_threads: Vec::new(),
        reviewed_revisions: std::collections::BTreeMap::new(),
        presentation: None,
        stack_snapshots: Vec::new(),
    };
    let anchor = DraftAnchor {
        kind: ReviewAnchorKind::Review,
        file_path: "<review>".to_string(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: None,
        line_kind: ReviewLineKind::Context,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    };
    let request = SavePublishRecordRequest {
        workspace: workspace.clone(),
        review_id: "review-1".to_string(),
        publisher_id: "github".to_string(),
        submitted: true,
        published_thread_anchors: vec![anchor.clone()],
        output: None,
        message: "submitted".to_string(),
    };
    save_publish_record_for_request(request, &config).expect("save publish record");

    let records = list_publish_records_for_request(
        &ListPublishRecordsRequest {
            repo_path: repo,
            workspace_id: workspace.id,
        },
        &config,
    )
    .expect("list publish records");

    assert_eq!(records.records.len(), 1);
    assert_eq!(records.records[0].published_thread_anchors, vec![anchor]);
}

#[test]
fn review_bundle_excludes_requested_thread_anchors() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git init")
            .success()
    );
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let anchor = DraftAnchor {
        kind: ReviewAnchorKind::Review,
        file_path: "<review>".to_string(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: None,
        line_kind: ReviewLineKind::Context,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    };
    save_draft_for_request(
        SaveDraftRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::Repository,
            scope: None,
            anchor: anchor.clone(),
            body: "Overall finding".to_string(),
        },
        &config,
    )
    .expect("save draft");

    let bundle = review_bundle_for_request(
        PublishReviewRequest {
            repo_path: repo,
            target: ReviewTarget::Repository,
            workspace: None,
            excluded_thread_anchors: vec![anchor],
            publisher_id: "local_markdown".to_string(),
            options: serde_json::Value::Null,
        },
        &config,
    )
    .expect("bundle");

    assert_eq!(
        bundle.threads,
        [] as [bcode_code_review_models::ReviewBundleThread; 0]
    );
}

#[test]
fn ai_exchange_delete_is_explicit_and_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_test(&repo, &["init", "--quiet"]);
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let exchange = ReviewAiExchange {
        schema_version: bcode_code_review_models::REVIEW_AI_EXCHANGE_SCHEMA_VERSION,
        exchange_id: "exchange-delete".to_string(),
        anchor: DraftAnchor {
            kind: ReviewAnchorKind::Review,
            file_path: "<review>".to_string(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: None,
            new_line: None,
            line_kind: ReviewLineKind::Context,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
            reply_to: None,
        },
        question: "delete me".to_string(),
        session_id: None,
        status: ReviewAiExchangeStatus::Pending,
        error: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    save_ai_exchange_for_request(
        SaveReviewAiExchangeRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::Repository,
            scope: None,
            exchange,
        },
        &config,
    )
    .expect("save exchange");
    let request = || DeleteReviewAiExchangeRequest {
        repo_path: repo.clone(),
        target: ReviewTarget::Repository,
        scope: None,
        exchange_id: "exchange-delete".to_string(),
    };
    assert!(
        delete_ai_exchange_for_request(request(), &config)
            .expect("delete")
            .deleted
    );
    assert!(
        !delete_ai_exchange_for_request(request(), &config)
            .expect("duplicate delete")
            .deleted
    );
}

#[test]
fn workspace_payloads_with_the_first_interface_presentation_state_still_decode() {
    let legacy: ReviewWorkspace = serde_json::from_value(serde_json::json!({
        "id": "legacy",
        "title": "Legacy",
        "repo_root": "/repo",
        "sources": [],
        "viewed_files": ["src/lib.rs"],
        "presentation_state": {
            "schema_version": 1,
            "selected_path": "src/lib.rs",
            "sidebar_mode": "threads",
        }
    }))
    .expect("legacy workspace");
    assert_eq!(
        legacy.viewed_files,
        BTreeSet::from(["src/lib.rs".to_owned()])
    );
    assert!(legacy.presentation.is_none());
}

/// A row with only the original workspace columns (and a retired presentation state, here
/// unreadable) lists, opens, and saves through the workspace operations.
#[test]
fn workspace_rows_with_only_the_original_columns_list_open_and_save() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let repo_root = resolve_repo_root(&repo).expect("repo root");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let sources =
        serde_json::to_string(&vec![test_source("staged", ReviewSourceKind::IndexStaged)])
            .expect("sources encode");
    let row_root = repo_root.display().to_string();
    with_database(&repo_root, &config, move |database| {
        Box::pin(async move {
            database
                .insert("review_workspaces")
                .value("workspace_id", "first-interface")
                .value("repo_root", row_root)
                .value("title", "Written before review state")
                .value("sources_json", sources)
                .value("created_at_ms", 1_i64)
                .value("updated_at_ms", 2_i64)
                .value("viewed_files_json", r#"["src/lib.rs"]"#)
                .value("presentation_state_json", "{not json")
                .execute(database)
                .await?;
            Ok(())
        })
    })
    .expect("insert first-interface row");

    let listed = list_review_workspaces_for_request(
        &ListReviewWorkspacesRequest {
            repo_path: repo.clone(),
            include_archived: false,
        },
        &config,
    )
    .expect("list");
    assert_eq!(listed.workspaces.len(), 1);
    let get = |repo_path: PathBuf| {
        get_review_workspace_for_request(
            GetReviewWorkspaceRequest {
                repo_path,
                workspace_id: "first-interface".to_owned(),
            },
            &config,
        )
        .expect("get")
        .workspace
        .expect("stored workspace")
    };
    let workspace = get(repo.clone());
    assert_eq!(workspace, listed.workspaces[0]);
    assert_eq!(workspace.title, "Written before review state");
    assert_eq!(
        workspace.viewed_files,
        BTreeSet::from(["src/lib.rs".to_owned()])
    );
    assert_eq!(workspace.viewed_marks, Vec::new());
    assert_eq!(workspace.reviewed_revisions, BTreeMap::new());
    assert!(workspace.presentation.is_none());

    let mut renamed = workspace;
    renamed.title = "Renamed later".to_owned();
    update_review_workspace_for_request(
        UpdateReviewWorkspaceRequest {
            repo_path: repo.clone(),
            workspace: renamed,
        },
        &config,
    )
    .expect("update");
    let reopened = get(repo.clone());
    assert_eq!(reopened.title, "Renamed later");
    assert_eq!(
        reopened.viewed_files,
        BTreeSet::from(["src/lib.rs".to_owned()])
    );

    stack_snapshots_round_trip(&repo, &repo_root, &config, reopened, get);
}

/// The stack a review last opened with is kept with it; an unreadable one is dropped.
fn stack_snapshots_round_trip(
    repo: &Path,
    repo_root: &Path,
    config: &CodeReviewConfig,
    reopened: ReviewWorkspace,
    get: impl Fn(PathBuf) -> ReviewWorkspace,
) {
    let repo = repo.to_path_buf();
    let config = config.clone();
    let repo_root = repo_root.to_path_buf();
    assert_eq!(reopened.stack_snapshots, Vec::new());
    let snapshot = bcode_code_review_models::ReviewStackSnapshot {
        provider: "github".into(),
        repository: "o/r".into(),
        stack: bcode_code_review_models::ReviewStack {
            trunk: "main".into(),
            rooted_at_default_branch: true,
            layers: Vec::new(),
            requested: 7,
            siblings: Vec::new(),
            warnings: Vec::new(),
        },
        confirmed_at_ms: 1_700_000_000_000,
    };
    let mut with_stack = reopened;
    with_stack.stack_snapshots = vec![snapshot.clone()];
    update_review_workspace_for_request(
        UpdateReviewWorkspaceRequest {
            repo_path: repo.clone(),
            workspace: with_stack,
        },
        &config,
    )
    .expect("update");
    assert_eq!(get(repo.clone()).stack_snapshots, [snapshot]);
    // An unreadable snapshot is dropped, not an error.
    with_database(&repo_root, &config, move |database| {
        Box::pin(async move {
            database
                .update("review_workspaces")
                .value("stack_snapshots_json", "{not json")
                .execute(database)
                .await?;
            Ok(())
        })
    })
    .expect("corrupt snapshot");
    assert_eq!(get(repo).stack_snapshots, Vec::new());
}

#[test]
fn ai_exchange_persistence_is_separate_from_publishable_drafts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git init")
            .success()
    );
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let anchor = DraftAnchor {
        kind: ReviewAnchorKind::Review,
        file_path: "<review>".to_string(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: None,
        line_kind: ReviewLineKind::Context,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    };
    let exchange = ReviewAiExchange {
        schema_version: bcode_code_review_models::REVIEW_AI_EXCHANGE_SCHEMA_VERSION,
        exchange_id: "exchange-1".to_string(),
        anchor,
        question: "Is this safe?".to_string(),
        session_id: Some("session-1".to_string()),
        status: ReviewAiExchangeStatus::Linked,
        error: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    };

    let saved = save_ai_exchange_for_request(
        SaveReviewAiExchangeRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::Repository,
            scope: None,
            exchange,
        },
        &config,
    )
    .expect("save exchange")
    .exchange;
    assert!(saved.created_at_ms > 0);

    let listed = list_ai_exchanges_for_request(
        &ListReviewAiExchangesRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::Repository,
            scope: None,
        },
        &config,
    )
    .expect("list exchanges");
    assert_eq!(listed.exchanges, vec![saved]);

    let bundle = review_bundle_for_request(
        PublishReviewRequest {
            repo_path: repo,
            target: ReviewTarget::Repository,
            workspace: None,
            excluded_thread_anchors: Vec::new(),
            publisher_id: "local_markdown".to_string(),
            options: serde_json::Value::Null,
        },
        &config,
    )
    .expect("bundle");
    assert_eq!(
        bundle.threads,
        [] as [bcode_code_review_models::ReviewBundleThread; 0]
    );
}

#[test]
fn suggestion_persistence_round_trips_and_updates_lifecycle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .expect("run git init");
    assert!(init.success());
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let anchor = DraftAnchor {
        kind: ReviewAnchorKind::Range,
        file_path: "src/lib.rs".to_string(),
        diff_row: 2,
        start_diff_row: Some(2),
        end_diff_row: Some(2),
        old_start: None,
        old_end: None,
        new_start: Some(1),
        new_end: Some(1),
        old_line: None,
        new_line: Some(1),
        line_kind: ReviewLineKind::Added,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    };
    let scope = Some(ReviewScope::Workspace {
        workspace_id: "workspace-1".to_string(),
        target: ReviewTarget::WorkingTreeUnstaged,
    });
    let suggestion = ReviewSuggestion {
        suggestion_id: "suggestion-1".to_string(),
        anchor,
        body: "Handle this error.".to_string(),
        session_id: Some("session-1".to_string()),
        status: ReviewSuggestionStatus::Suggested,
        rationale: Some("AI review".to_string()),
        created_at_ms: 0,
        updated_at_ms: 0,
    };

    let saved = save_suggestion_for_request(
        SaveReviewSuggestionRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::WorkingTreeUnstaged,
            scope: scope.clone(),
            suggestion,
        },
        &config,
    )
    .expect("save suggestion")
    .suggestion;
    assert!(saved.created_at_ms > 0);
    assert_eq!(saved.created_at_ms, saved.updated_at_ms);

    let listed = list_suggestions_for_request(
        &ListReviewSuggestionsRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::WorkingTreeUnstaged,
            scope: scope.clone(),
        },
        &config,
    )
    .expect("list suggestions");
    assert_eq!(listed.suggestions, vec![saved.clone()]);

    let mut rejected = saved.clone();
    rejected.status = ReviewSuggestionStatus::Rejected;
    let updated = save_suggestion_for_request(
        SaveReviewSuggestionRequest {
            repo_path: repo.clone(),
            target: ReviewTarget::WorkingTreeUnstaged,
            scope: scope.clone(),
            suggestion: rejected,
        },
        &config,
    )
    .expect("update suggestion")
    .suggestion;
    assert_eq!(updated.status, ReviewSuggestionStatus::Rejected);
    assert_eq!(updated.created_at_ms, saved.created_at_ms);
    assert!(updated.updated_at_ms >= saved.updated_at_ms);

    let reopened = list_suggestions_for_request(
        &ListReviewSuggestionsRequest {
            repo_path: repo,
            target: ReviewTarget::WorkingTreeUnstaged,
            scope,
        },
        &config,
    )
    .expect("reopen suggestions");
    assert_eq!(reopened.suggestions, vec![updated]);
}

#[test]
fn repo_root_resolution_rejects_relative_paths() {
    for relative in [".", "./nested", "nested", ".."] {
        let error = resolve_repo_root(Path::new(relative))
            .expect_err("relative repo_path should be rejected");

        match error {
            StoreError::InvalidRequest(message) => assert!(
                message.contains("must be absolute"),
                "expected an absolute-path rejection for {relative:?}, got {message}"
            ),
            other => panic!("expected InvalidRequest for {relative:?}, got {other:?}"),
        }
    }
}

#[test]
fn repo_root_resolution_accepts_absolute_repository() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());

    let repo_root = resolve_repo_root(&repo).expect("absolute repository resolves");

    assert!(repo_root.is_absolute());
}

#[test]
fn default_state_path_uses_user_bcode_state_dir() {
    let config = CodeReviewConfig::default();
    let state_paths = state_paths(Path::new("/repo"), &config);

    assert_eq!(
        state_paths.state_root,
        bcode_config::default_state_dir().join(DEFAULT_STATE_SUBDIR)
    );
}

#[test]
fn repo_state_location_uses_repo_local_directory() {
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::Repo,
        state_dir: None,
    };
    let state_paths = state_paths(Path::new("/repo"), &config);

    assert_eq!(
        state_paths.state_root,
        PathBuf::from("/repo/.bcode/code-review")
    );
}

#[test]
fn explicit_relative_state_dir_is_repo_relative() {
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(PathBuf::from(".bcode/code-review")),
    };
    let state_paths = state_paths(Path::new("/repo"), &config);

    assert_eq!(
        state_paths.state_root,
        PathBuf::from("/repo/.bcode/code-review")
    );
}

#[test]
fn explicit_absolute_state_dir_is_used() {
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(PathBuf::from("/state/code-review")),
    };
    let state_paths = state_paths(Path::new("/repo"), &config);

    assert_eq!(state_paths.state_root, PathBuf::from("/state/code-review"));
}

#[test]
fn parses_modified_file_diff() {
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n line one\n-old\n+new\n+extra\n";

    let files = parse_unified_diff(diff).expect("parse diff");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].display_path(), "src/lib.rs");
    assert_eq!(files[0].status, ReviewFileStatus::Modified);
    assert_eq!(files[0].additions, 2);
    assert_eq!(files[0].deletions, 1);
    assert_eq!(files[0].hunks.len(), 1);
    assert_eq!(files[0].hunks[0].lines.len(), 4);
    assert_eq!(files[0].hunks[0].lines[0].old_line, Some(1));
    assert_eq!(files[0].hunks[0].lines[0].new_line, Some(1));
}

#[test]
fn parses_rename_diff() {
    let diff = "diff --git a/old.rs b/new.rs\nsimilarity index 90%\nrename from old.rs\nrename to new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n";

    let files = parse_unified_diff(diff).expect("parse diff");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
    assert_eq!(files[0].new_path.as_deref(), Some("new.rs"));
    assert_eq!(files[0].status, ReviewFileStatus::Renamed);
    assert_eq!(files[0].additions, 1);
    assert_eq!(files[0].deletions, 1);
}

#[test]
fn publisher_registry_lists_builtin_publishers() {
    let publishers = builtin_publishers();

    assert_eq!(publishers.len(), 3);
    assert!(
        publishers
            .iter()
            .any(|publisher| publisher.id == "local_markdown")
    );
    assert!(
        publishers
            .iter()
            .any(|publisher| publisher.id == "markdown_file")
    );
    assert!(
        publishers
            .iter()
            .any(|publisher| publisher.id == "json_file")
    );
}

#[test]
fn local_markdown_publisher_preview_is_local_review_report() {
    let bundle = ReviewBundle {
        pull_request: None,
        review_id: "review-1".to_string(),
        title: "Review".to_string(),
        repo_root: PathBuf::from("/repo"),
        target: ReviewTarget::WorkingTreeUnstaged,
        surfaces: Vec::new(),
        files: vec![ReviewFileSummary {
            path: "src/lib.rs".to_string(),
            status: ReviewFileStatus::Modified,
            additions: 2,
            deletions: 1,
            hunks: 1,
            is_binary: false,
        }],
        threads: vec![ReviewBundleThread {
            summary_only: false,
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                kind: ReviewAnchorKind::Range,
                file_path: "src/lib.rs".to_string(),
                diff_row: 3,
                old_line: None,
                new_line: Some(2),
                start_diff_row: Some(3),
                end_diff_row: Some(3),
                old_start: None,
                old_end: None,
                new_start: Some(2),
                new_end: Some(2),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
                head_oid: None,
                blob_oid: None,
                reply_to: None,
            },
            comments: vec![DraftComment {
                comment_id: "comment-1".to_string(),
                thread_id: "thread-1".to_string(),
                anchor: DraftAnchor {
                    kind: ReviewAnchorKind::Range,
                    file_path: "src/lib.rs".to_string(),
                    diff_row: 3,
                    old_line: None,
                    new_line: Some(2),
                    start_diff_row: Some(3),
                    end_diff_row: Some(3),
                    old_start: None,
                    old_end: None,
                    new_start: Some(2),
                    new_end: Some(2),
                    line_kind: ReviewLineKind::Added,
                    is_file_anchor: false,
                    surface_id: None,
                    source_id: None,
                    head_oid: None,
                    blob_oid: None,
                    reply_to: None,
                },
                body: "Needs local follow-up".to_string(),
                created_at_ms: 1,
                updated_at_ms: 1,
                session_id: Some("session-1".to_string()),
                resolved_at_ms: None,
                thread_kind: ReviewThreadKind::Note,
                severity: ReviewThreadSeverity::Info,
            }],
            session_id: Some("session-1".to_string()),
            resolved_at_ms: None,
            selected_lines: Vec::new(),
            selected_diff_lines: vec!["+let x = 1;".to_string()],
            hunk_context: Vec::new(),
        }],
        generated_at_ms: 1,
    };

    let preview = with_publisher("local_markdown", |publisher| {
        publisher.preview(&bundle, &serde_json::json!({}))
    })
    .expect("local markdown preview");

    assert!(preview.contains("# Local review: Review"));
    assert!(preview.contains("* Open threads: `1`"));
    assert!(preview.contains("## File threads"));
    assert!(preview.contains("### `src/lib.rs` row 3 (open)"));
    assert!(preview.contains("* Bcode session: `session-1`"));
    assert!(preview.contains("Needs local follow-up"));
}

#[test]
fn local_markdown_groups_review_level_threads() {
    let bundle = ReviewBundle {
        pull_request: None,
        review_id: "review-1".to_string(),
        title: "Review".to_string(),
        repo_root: PathBuf::from("/repo"),
        target: ReviewTarget::WorkingTreeUnstaged,
        surfaces: Vec::new(),
        files: Vec::new(),
        threads: vec![ReviewBundleThread {
            summary_only: false,
            thread_id: "thread-review".to_string(),
            anchor: DraftAnchor {
                kind: ReviewAnchorKind::Review,
                file_path: "<review>".to_string(),
                diff_row: 0,
                old_line: None,
                new_line: None,
                start_diff_row: None,
                end_diff_row: None,
                old_start: None,
                old_end: None,
                new_start: None,
                new_end: None,
                line_kind: ReviewLineKind::Context,
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
                head_oid: None,
                blob_oid: None,
                reply_to: None,
            },
            comments: vec![DraftComment {
                comment_id: "comment-review".to_string(),
                thread_id: "thread-review".to_string(),
                anchor: DraftAnchor {
                    kind: ReviewAnchorKind::Review,
                    file_path: "<review>".to_string(),
                    diff_row: 0,
                    old_line: None,
                    new_line: None,
                    start_diff_row: None,
                    end_diff_row: None,
                    old_start: None,
                    old_end: None,
                    new_start: None,
                    new_end: None,
                    line_kind: ReviewLineKind::Context,
                    is_file_anchor: false,
                    surface_id: None,
                    source_id: None,
                    head_oid: None,
                    blob_oid: None,
                    reply_to: None,
                },
                body: "Overall review summary".to_string(),
                created_at_ms: 1,
                updated_at_ms: 1,
                session_id: None,
                resolved_at_ms: None,
                thread_kind: ReviewThreadKind::Note,
                severity: ReviewThreadSeverity::Info,
            }],
            session_id: None,
            resolved_at_ms: None,
            selected_lines: Vec::new(),
            selected_diff_lines: Vec::new(),
            hunk_context: Vec::new(),
        }],
        generated_at_ms: 1,
    };

    let preview = with_publisher("local_markdown", |publisher| {
        publisher.preview(&bundle, &serde_json::json!({}))
    })
    .expect("local markdown preview");

    assert!(preview.contains("## Review-level threads"));
    assert!(preview.contains("### Review (open)"));
    assert!(preview.contains("Overall review summary"));
    assert!(!preview.contains("## File threads"));
}

#[test]
fn json_publisher_preview_is_review_bundle_json() {
    let bundle = ReviewBundle {
        pull_request: None,
        review_id: "review-1".to_string(),
        title: "Review".to_string(),
        repo_root: PathBuf::from("/repo"),
        target: ReviewTarget::WorkingTreeUnstaged,
        surfaces: Vec::new(),
        files: Vec::new(),
        threads: Vec::new(),
        generated_at_ms: 1,
    };

    let preview = with_publisher("json_file", |publisher| {
        publisher.preview(&bundle, &serde_json::json!({}))
    })
    .expect("json preview");
    let decoded: ReviewBundle = serde_json::from_str(&preview).expect("valid bundle json");

    assert_eq!(decoded.review_id, "review-1");
}

#[test]
fn unsupported_publisher_returns_error() {
    let error = with_publisher("missing", |publisher| {
        publisher.preview(
            &ReviewBundle {
                pull_request: None,
                review_id: "review-1".to_string(),
                title: "Review".to_string(),
                repo_root: PathBuf::from("/repo"),
                target: ReviewTarget::WorkingTreeUnstaged,
                surfaces: Vec::new(),
                files: Vec::new(),
                threads: Vec::new(),
                generated_at_ms: 1,
            },
            &serde_json::json!({}),
        )
    })
    .expect_err("missing publisher should fail");

    assert!(matches!(error, StoreError::UnsupportedPublisher(_)));
}

#[test]
fn markdown_export_writes_selected_publish_bundle_to_requested_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("selected-review.md");
    let bundle = ReviewBundle {
        pull_request: None,
        review_id: "review-selection".to_string(),
        title: "Selected review".to_string(),
        repo_root: temp.path().to_path_buf(),
        target: ReviewTarget::WorkingTreeUnstaged,
        surfaces: Vec::new(),
        files: Vec::new(),
        threads: vec![ReviewBundleThread {
            summary_only: false,
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                kind: ReviewAnchorKind::Range,
                file_path: "src/lib.rs".to_string(),
                diff_row: 1,
                start_diff_row: None,
                end_diff_row: None,
                old_start: None,
                old_end: None,
                new_start: Some(42),
                new_end: Some(42),
                old_line: None,
                new_line: Some(42),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
                head_oid: None,
                blob_oid: None,
                reply_to: None,
            },
            comments: vec![DraftComment {
                comment_id: "comment-1".to_string(),
                thread_id: "thread-1".to_string(),
                anchor: DraftAnchor {
                    kind: ReviewAnchorKind::Range,
                    file_path: "src/lib.rs".to_string(),
                    diff_row: 1,
                    start_diff_row: None,
                    end_diff_row: None,
                    old_start: None,
                    old_end: None,
                    new_start: Some(42),
                    new_end: Some(42),
                    old_line: None,
                    new_line: Some(42),
                    line_kind: ReviewLineKind::Added,
                    is_file_anchor: false,
                    surface_id: None,
                    source_id: None,
                    head_oid: None,
                    blob_oid: None,
                    reply_to: None,
                },
                body: "Selected actionable comment".to_string(),
                created_at_ms: 1,
                updated_at_ms: 1,
                session_id: None,
                resolved_at_ms: None,
                thread_kind: ReviewThreadKind::Finding,
                severity: ReviewThreadSeverity::Warning,
            }],
            resolved_at_ms: None,
            session_id: None,
            selected_lines: Vec::new(),
            selected_diff_lines: Vec::new(),
            hunk_context: Vec::new(),
        }],
        generated_at_ms: 1,
    };

    let response = with_publisher("markdown_file", |publisher| {
        publisher.submit(&bundle, &serde_json::json!({ "output_path": output }))
    })
    .expect("markdown export");
    let exported = std::fs::read_to_string(&output).expect("exported markdown");

    assert!(response.submitted);
    assert_eq!(response.output.as_deref(), output.to_str());
    assert!(exported.contains("Selected review"));
    assert!(exported.contains("Selected actionable comment"));
    assert!(!exported.contains("review AI exchange"));
}

#[test]
fn output_path_override_is_used() {
    let bundle = ReviewBundle {
        pull_request: None,
        review_id: "review-1".to_string(),
        title: "Review".to_string(),
        repo_root: PathBuf::from("/repo"),
        target: ReviewTarget::WorkingTreeUnstaged,
        surfaces: Vec::new(),
        files: Vec::new(),
        threads: Vec::new(),
        generated_at_ms: 1,
    };
    let path = publish_output_path(
        &bundle,
        &serde_json::json!({ "output_path": "custom/review.json" }),
        "json",
    );

    assert_eq!(path, PathBuf::from("custom/review.json"));
}

/// Opening a fresh state database from many callers at once (the app and the daemon's plugin,
/// or several requests of one process) migrates it once; no caller fails on a schema change
/// another caller already applied.
#[test]
fn concurrent_first_opens_of_a_fresh_database_all_succeed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let repo_root = resolve_repo_root(&repo).expect("repo root");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let (repo_root, config, barrier) = (repo_root.clone(), config.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                with_database(&repo_root, &config, |database| {
                    Box::pin(async move { Ok(database.table_exists("draft_threads").await?) })
                })
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        let opened = handle.join().expect("caller thread");
        assert!(opened.expect("fresh database opens"), "schema is migrated");
    }
}

#[test]
fn the_version_log_merges_what_each_source_saw() {
    use bcode_code_review_models::{
        LayerVersion, ListLayerVersionsRequest, RecordLayerVersionsRequest, VersionSource,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let version = |number: u64, head: &str, at: u64, source: VersionSource| LayerVersion {
        provider: "github".into(),
        repository: "o/r".into(),
        number,
        head_oid: head.into(),
        base_oid: None,
        base_branch: None,
        seen_at_ms: at,
        actor: None,
        sources: BTreeSet::from([source]),
    };
    let record = |versions: Vec<LayerVersion>| {
        super::history::record_layer_versions_for_request(
            RecordLayerVersionsRequest {
                repo_path: repo.clone(),
                versions,
            },
            &config,
        )
        .expect("record")
        .added
    };
    assert_eq!(
        record(vec![
            version(1, "a1", 200, VersionSource::Observed),
            version(1, "a2", 300, VersionSource::Observed),
            version(2, "b1", 100, VersionSource::Observed),
        ]),
        3
    );
    // The forge saw a1 earlier, and knows who pushed it and on what.
    let mut pushed = version(1, "a1", 150, VersionSource::Forge);
    pushed.actor = Some("octocat".into());
    pushed.base_branch = Some("main".into());
    assert_eq!(record(vec![pushed]), 0);
    let listed = super::history::list_layer_versions_for_request(
        &ListLayerVersionsRequest {
            repo_path: repo.clone(),
            provider: "github".into(),
            repository: "o/r".into(),
            numbers: vec![1],
        },
        &config,
    )
    .expect("list")
    .versions;
    let heads: Vec<&str> = listed
        .iter()
        .map(|version| version.head_oid.as_str())
        .collect();
    assert_eq!(heads, ["a1", "a2"]);
    assert_eq!(listed[0].seen_at_ms, 150);
    assert_eq!(listed[0].actor.as_deref(), Some("octocat"));
    assert_eq!(listed[0].base_branch.as_deref(), Some("main"));
    assert_eq!(
        listed[0].sources,
        BTreeSet::from([VersionSource::Forge, VersionSource::Observed])
    );
}
