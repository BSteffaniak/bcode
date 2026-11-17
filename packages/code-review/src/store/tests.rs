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
            base: None,
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
            base: None,
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

#[test]
fn a_reviews_pull_request_threads_are_kept_per_scope_and_replaced() {
    use bcode_code_review_models::{
        GetReviewConversationRequest, Participant, ParticipantKind, PullThreadRecord, RemoteAnchor,
        RemoteComment, RemoteThread, ReviewConversationSnapshot, SaveReviewConversationRequest,
        ThreadPlacement,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let store = ReviewStore::new(CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    });
    let scope = |id: &str| {
        Some(ReviewScope::Workspace {
            workspace_id: id.to_owned(),
            target: ReviewTarget::WorkingTreeUnstaged,
        })
    };
    let get = |id: &str| {
        store
            .conversation(&GetReviewConversationRequest {
                repo_path: repo.clone(),
                target: ReviewTarget::WorkingTreeUnstaged,
                scope: scope(id),
            })
            .expect("get")
            .snapshot
    };
    assert_eq!(get("a"), None);
    let record = |body: String| PullThreadRecord {
        id: "o/r#7:T1".to_owned(),
        provider: "github".to_owned(),
        provider_label: "GitHub".to_owned(),
        repository: "o/r".to_owned(),
        number: 7,
        location: "the review".to_owned(),
        placement: ThreadPlacement::OnLayer,
        thread: RemoteThread {
            id: "T1".to_owned(),
            anchor: RemoteAnchor {
                path: "src/a.rs".to_owned(),
                original_line: Some(4),
                diff_hunk: Some("@@ -1,2 +1,4 @@\n+a".to_owned()),
                ..RemoteAnchor::default()
            },
            resolved: false,
            outdated: true,
            can_reply: true,
            can_resolve: true,
            comments: vec![RemoteComment {
                id: "C1".to_owned(),
                author: Participant {
                    login: "kevin".to_owned(),
                    name: None,
                    kind: ParticipantKind::User,
                    url: None,
                    is_viewer: false,
                },
                body,
                created_at_ms: Some(1),
                updated_at_ms: None,
                url: None,
                review_id: None,
            }],
        },
    };
    let save = |id: &str, body: &str, at: u64| {
        store
            .save_conversation(SaveReviewConversationRequest {
                repo_path: repo.clone(),
                target: ReviewTarget::WorkingTreeUnstaged,
                scope: scope(id),
                snapshot: ReviewConversationSnapshot {
                    threads: vec![record(body.to_owned())],
                    omitted: 0,
                    read_at_ms: at,
                },
            })
            .expect("save")
            .saved
    };
    assert_eq!(save("a", "first", 10), 1);
    assert_eq!(save("a", &"x".repeat(40 * 1024), 20), 1);
    save("b", "other review", 30);
    let kept = get("a").expect("saved");
    assert_eq!(
        kept.read_at_ms, 20,
        "the latest read replaces the earlier one"
    );
    let body = &kept.threads[0].thread.comments[0].body;
    assert!(
        body.len() < 17 * 1024 && body.ends_with('…'),
        "bounded when saved"
    );
    let hunk = kept.threads[0].thread.anchor.diff_hunk.as_deref();
    assert_eq!(hunk, Some("@@ -1,2 +1,4 @@\n+a"));
    let other = get("b").expect("saved");
    assert_eq!(other.threads[0].thread.comments[0].body, "other review");
}

#[test]
fn looks_upsert_by_head_and_kind_seed_idempotently_and_prune_old_heads() {
    use crate::history::looks::pass_looks;
    use bcode_code_review_models::{
        LayerLook, ListLayerLooksRequest, LookKind, MAX_LOOK_HEADS_PER_PULL,
        RecordLayerLooksRequest, ReviewedRevision,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let store = ReviewStore::new(CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    });
    let look = |number: u64, head: &str, kind: LookKind, at: u64| LayerLook {
        provider: "github".into(),
        repository: "o/r".into(),
        number,
        head_oid: head.into(),
        fork_oid: None,
        kind,
        looked_at_ms: at,
        review_id: None,
    };
    let record = |looks: Vec<LayerLook>| {
        store
            .record_layer_looks(RecordLayerLooksRequest {
                repo_path: repo.clone(),
                looks,
            })
            .expect("record")
    };
    let list = |numbers: Vec<u64>| {
        store
            .list_layer_looks(&ListLayerLooksRequest {
                repo_path: repo.clone(),
                provider: "github".into(),
                repository: "o/r".into(),
                numbers,
            })
            .expect("list")
            .looks
    };
    assert_eq!(
        record(vec![
            look(1, "a", LookKind::Viewed, 100),
            look(1, "a", LookKind::Opened, 90),
            look(2, "b", LookKind::Commented, 50),
        ])
        .recorded,
        3
    );
    // A repeated look moves its time forward and learns its fork point; an older one changes
    // nothing.
    let mut later = look(1, "a", LookKind::Viewed, 200);
    later.fork_oid = Some("f".into());
    assert_eq!(
        record(vec![later, look(1, "a", LookKind::Opened, 10)]).recorded,
        1
    );
    let looks = list(vec![1]);
    assert_eq!(looks.len(), 2);
    let viewed = looks
        .iter()
        .find(|look| look.kind == LookKind::Viewed)
        .expect("viewed");
    assert_eq!(
        (viewed.looked_at_ms, viewed.fork_oid.as_deref()),
        (200, Some("f"))
    );

    // Seeding from completed passes is idempotent.
    let reviewed = [(
        2,
        ReviewedRevision {
            reviewed_base_oid: Some("base".into()),
            reviewed_head_oid: Some("b0".into()),
            recorded_at_ms: Some(40),
        },
    )];
    let seeded = pass_looks("github", "o/r", &reviewed, Some("w1"));
    assert_eq!(record(seeded.clone()).recorded, 1);
    assert_eq!(record(seeded).recorded, 0);
    assert_eq!(list(vec![2]).len(), 2);

    // Past the cap, the heads looked at longest ago are pruned and reported.
    let many: Vec<LayerLook> = (0..MAX_LOOK_HEADS_PER_PULL + 2)
        .map(|index| {
            look(
                3,
                &format!("h{index:02}"),
                LookKind::Viewed,
                1_000 + index as u64,
            )
        })
        .collect();
    let response = record(many);
    assert_eq!(response.pruned_heads, ["h01", "h00"]);
    let kept = list(vec![3]);
    assert_eq!(kept.len(), MAX_LOOK_HEADS_PER_PULL);
    assert!(kept.iter().all(|look| look.head_oid != "h00"));
}

#[test]
fn assistant_bindings_keep_where_they_started_and_list_and_unbind() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_test(&repo, &["init", "--quiet"]);
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let workspace = |id: &str, title: &str| {
        let mut workspace = test_workspace(&repo, Vec::new());
        workspace.id = id.to_owned();
        workspace.title = title.to_owned();
        workspace
    };
    let bind = |session: &str, workspace: ReviewWorkspace, detached: bool| {
        bind_assistant_for_request(
            BindReviewAssistantRequest {
                repo_path: repo.clone(),
                binding: ReviewAssistantBinding {
                    schema_version:
                        bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                    session_id: session.to_owned(),
                    workspace,
                    resolved: Vec::new(),
                    stacks: bcode_code_review_models::ReviewAssistantStacks::default(),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    started: None,
                    detached_at_ms: detached.then_some(1),
                    guide_grant: None,
                },
            },
            &config,
        )
        .expect("bind")
        .binding
    };

    // First saved in a review: it started there, and rebinding elsewhere keeps that.
    let first = bind("s-1", workspace("rv-1", "Retry"), false);
    let started = first.started.expect("started");
    assert_eq!(started.workspace_id.as_deref(), Some("rv-1"));
    assert_eq!(started.title.as_deref(), Some("Retry"));
    let moved = bind("s-1", workspace("rv-2", "Auth"), false);
    assert_eq!(moved.started, Some(started.clone()));
    let detached = bind("s-1", workspace("rv-2", "Auth"), true);
    assert_eq!(detached.started, Some(started));
    assert!(detached.is_detached());

    // First saved detached: it started where no review was on screen.
    let home = bind("s-2", workspace("rv-1", "Retry"), true);
    assert_eq!(home.started, Some(ReviewAssistantTag::none()));
    // A detached session is never the newest binding of a review.
    let newest = assistant_binding_for_request(
        &GetReviewAssistantRequest {
            repo_path: repo.clone(),
            lookup: ReviewAssistantLookup::Review {
                workspace_id: "rv-2".to_owned(),
            },
        },
        &config,
    )
    .expect("lookup")
    .binding;
    assert!(newest.is_none(), "{newest:?}");

    std::thread::sleep(std::time::Duration::from_millis(2));
    bind("s-3", workspace("rv-1", "Retry"), false);
    let list = |limit| {
        list_assistants_for_request(
            &bcode_code_review_models::ListReviewAssistantsRequest {
                repo_path: repo.clone(),
                limit,
            },
            &config,
        )
        .expect("list")
    };
    let listed = list(None);
    assert_eq!(
        listed.bindings.first().map(|b| b.session_id.as_str()),
        Some("s-3")
    );
    assert_eq!(listed.bindings.len(), 3);
    assert!(!listed.truncated);
    let bounded = list(Some(2));
    assert_eq!(bounded.bindings.len(), 2);
    assert!(bounded.truncated);

    let unbind = || {
        unbind_assistant_for_request(
            &bcode_code_review_models::UnbindReviewAssistantRequest {
                repo_path: repo.clone(),
                session_id: "s-1".to_owned(),
            },
            &config,
        )
        .expect("unbind")
        .removed
    };
    assert!(unbind());
    assert!(!unbind(), "unbinding again is a no-op");
    assert_eq!(list(None).bindings.len(), 2);
}

#[test]
fn assistant_rows_written_before_tags_existed_are_tagged_with_their_review_when_moved() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_test(&repo, &["init", "--quiet"]);
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let mut workspace = test_workspace(&repo, Vec::new());
    workspace.id = "rv-legacy".to_owned();
    workspace.title = "Legacy".to_owned();
    // A row as an earlier version wrote it: no `started`, no `detached_at_ms` fields.
    let legacy = serde_json::json!({
        "schema_version": bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
        "session_id": "old",
        "workspace": workspace,
        "created_at_ms": 5,
        "updated_at_ms": 5,
    });
    let resolved = resolve_repo_root(&repo).expect("root");
    let db_root = resolved.clone();
    with_database(&resolved, &config, move |database| {
        Box::pin(async move {
            database
                .insert("review_assistant_sessions")
                .value("session_id", "old".to_owned())
                .value("workspace_id", "rv-legacy".to_owned())
                .value("repo_root", db_root.display().to_string())
                .value("binding_json", legacy.to_string())
                .value("created_at_ms", 5_i64)
                .value("updated_at_ms", 5_i64)
                .execute(database)
                .await?;
            Ok::<_, StoreError>(())
        })
    })
    .expect("legacy row");
    let listed = list_assistants_for_request(
        &bcode_code_review_models::ListReviewAssistantsRequest {
            repo_path: repo.clone(),
            limit: None,
        },
        &config,
    )
    .expect("list");
    assert_eq!(listed.bindings.len(), 1, "legacy rows are listed");
    assert_eq!(listed.bindings[0].started, None);

    let mut moved = listed.bindings[0].clone();
    moved.workspace.id = "rv-new".to_owned();
    let moved = bind_assistant_for_request(
        BindReviewAssistantRequest {
            repo_path: repo,
            binding: moved,
        },
        &config,
    )
    .expect("rebind")
    .binding;
    assert_eq!(
        moved.started.and_then(|started| started.workspace_id),
        Some("rv-legacy".to_owned())
    );
    assert_eq!(moved.created_at_ms, 5);
}

/// A store over a fresh repository and state directory, with one saved review.
fn store_with_review(temp: &Path) -> (ReviewStore, PathBuf, ReviewWorkspace) {
    let repo = initialized_test_repository(&temp.join("repo"));
    let store = ReviewStore::new(CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.join("state")),
    });
    let created = store
        .create_workspace(CreateReviewWorkspaceRequest {
            repo_path: repo.clone(),
            title: Some("Original".to_owned()),
            sources: vec![test_source("last", ReviewSourceKind::LastCommit)],
            presentation: None,
        })
        .expect("create")
        .workspace;
    (store, repo, created)
}

fn stored(store: &ReviewStore, repo: &Path, id: &str) -> Option<ReviewWorkspace> {
    store
        .get_workspace(GetReviewWorkspaceRequest {
            repo_path: repo.to_path_buf(),
            workspace_id: id.to_owned(),
        })
        .expect("get")
        .workspace
}

fn write_from(
    store: &ReviewStore,
    repo: &Path,
    base: &ReviewWorkspace,
    local: ReviewWorkspace,
) -> Result<ReviewWorkspace, StoreError> {
    store
        .update_workspace(UpdateReviewWorkspaceRequest {
            repo_path: repo.to_path_buf(),
            workspace: local,
            base: Some(base.clone()),
        })
        .map(|response| response.workspace)
}

#[test]
fn a_save_from_an_older_copy_keeps_renames_archives_and_marks_made_elsewhere() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("repo")).expect("repo dir");
    let (store, repo, opened) = store_with_review(temp.path());
    // Elsewhere (the assistant's tools): a rename, a viewed mark, then an archive.
    let mut renamed = opened.clone();
    renamed.title = "Renamed by the assistant".to_owned();
    write_from(&store, &repo, &opened, renamed).expect("rename");
    let mut marked = opened.clone();
    marked.viewed_files.insert("file.txt".to_owned());
    write_from(&store, &repo, &opened, marked).expect("mark");
    assert!(
        store
            .archive_workspace(ArchiveReviewWorkspaceRequest {
                repo_path: repo.clone(),
                workspace_id: opened.id.clone(),
            })
            .expect("archive")
            .archived
    );
    // The open review still holds the copy it opened with and saves its presentation.
    let mut local = opened.clone();
    local.presentation = Some(bcode_code_review_models::ReviewPresentationState {
        hide_resolved_threads: true,
        ..Default::default()
    });
    let written = write_from(&store, &repo, &opened, local).expect("save");
    let now = stored(&store, &repo, &opened.id).expect("still saved");
    assert_eq!(written, now, "the answer is what the store holds");
    assert_eq!(now.title, "Renamed by the assistant");
    assert!(
        now.archived_at_ms.is_some(),
        "the archive survives the save"
    );
    assert!(
        now.viewed_files.contains("file.txt"),
        "the mark survives the save"
    );
    assert!(
        now.presentation
            .as_ref()
            .is_some_and(|state| state.hide_resolved_threads),
        "the save's own change is written"
    );
}

#[test]
fn a_save_never_brings_back_a_deleted_review() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("repo")).expect("repo dir");
    let (store, repo, opened) = store_with_review(temp.path());
    assert!(store.delete_workspace(&repo, &opened.id).expect("delete"));
    let mut local = opened.clone();
    local.viewed_files.insert("file.txt".to_owned());
    let error = write_from(&store, &repo, &opened, local).expect_err("deleted");
    assert!(matches!(error, StoreError::WorkspaceDeleted(ref id) if *id == opened.id));
    assert_eq!(stored(&store, &repo, &opened.id), None);
}

#[test]
fn concurrent_writers_from_one_copy_each_keep_their_marks() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("repo")).expect("repo dir");
    let (store, repo, opened) = store_with_review(temp.path());
    let writers: Vec<_> = (0..8)
        .map(|index| {
            let (store, repo, opened) = (store.clone(), repo.clone(), opened.clone());
            std::thread::spawn(move || {
                let mut local = opened.clone();
                local.viewed_files.insert(format!("file{index}.rs"));
                if index == 0 {
                    local.title = "Renamed".to_owned();
                }
                write_from(&store, &repo, &opened, local).expect("write");
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer");
    }
    let now = stored(&store, &repo, &opened.id).expect("saved");
    assert_eq!(
        now.viewed_files,
        (0..8)
            .map(|index| format!("file{index}.rs"))
            .collect::<BTreeSet<_>>(),
        "no writer undid another's mark"
    );
    assert_eq!(now.title, "Renamed");
}

#[test]
fn assistant_settings_are_one_record_shared_by_every_repository_of_the_store() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let repo = |name: &str| {
        let repo = temp.path().join(name);
        std::fs::create_dir_all(&repo).expect("repo");
        git_test(&repo, &["init", "--quiet"]);
        repo
    };
    let (first, second) = (repo("first"), repo("second"));
    let read = |repo: &std::path::Path| {
        super::assistant_settings::assistant_settings_for_request(
            &bcode_code_review_models::GetReviewAssistantSettingsRequest {
                repo_path: repo.to_path_buf(),
            },
            &config,
        )
        .expect("read")
        .settings
    };
    assert_eq!(read(&first), None, "nothing saved yet");
    let saved = super::assistant_settings::save_assistant_settings_for_request(
        bcode_code_review_models::SaveReviewAssistantSettingsRequest {
            repo_path: first.clone(),
            settings: bcode_code_review_models::ReviewAssistantSettings {
                provider_plugin_id: Some("bcode.fake-provider".to_owned()),
                model_id: Some("fake-echo".to_owned()),
                effort: Some("high".to_owned()),
                updated_at_ms: 0,
            },
        },
        &config,
    )
    .expect("save")
    .settings;
    assert!(saved.updated_at_ms > 0);
    assert_eq!(read(&second), Some(saved), "the store's one record");
    // Saving again replaces it; an empty record means Bcode's defaults.
    super::assistant_settings::save_assistant_settings_for_request(
        bcode_code_review_models::SaveReviewAssistantSettingsRequest {
            repo_path: second,
            settings: bcode_code_review_models::ReviewAssistantSettings::default(),
        },
        &config,
    )
    .expect("save");
    assert!(read(&first).is_some_and(|settings| settings.is_empty()));
}

#[test]
fn the_assistant_panel_size_is_its_own_record_beside_the_model_settings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let repo = initialized_test_repository(temp.path());
    let read_size = || {
        super::assistant_settings::assistant_panel_size_for_request(
            &bcode_code_review_models::GetReviewAssistantPanelSizeRequest {
                repo_path: repo.clone(),
            },
            &config,
        )
        .expect("read")
        .size
    };
    let save_size = |size| {
        super::assistant_settings::save_assistant_panel_size_for_request(
            &bcode_code_review_models::SaveReviewAssistantPanelSizeRequest {
                repo_path: repo.clone(),
                size,
            },
            &config,
        )
        .expect("save")
        .size
    };
    assert_eq!(read_size(), None, "nothing saved yet");
    let saved = save_size(bcode_code_review_models::ReviewAssistantPanelSize {
        right_share: Some(6_000),
        bottom_share: None,
        updated_at_ms: 0,
    });
    assert!(saved.updated_at_ms > 0);
    assert_eq!(read_size(), Some(saved));
    // Saving the model settings leaves the size alone, and the other way round.
    let settings = super::assistant_settings::save_assistant_settings_for_request(
        bcode_code_review_models::SaveReviewAssistantSettingsRequest {
            repo_path: repo.clone(),
            settings: bcode_code_review_models::ReviewAssistantSettings {
                model_id: Some("fake-echo".to_owned()),
                ..bcode_code_review_models::ReviewAssistantSettings::default()
            },
        },
        &config,
    )
    .expect("save")
    .settings;
    assert_eq!(read_size(), Some(saved));
    let reset = save_size(bcode_code_review_models::ReviewAssistantPanelSize::default());
    assert_eq!(read_size(), Some(reset));
    assert_eq!(
        super::assistant_settings::assistant_settings_for_request(
            &bcode_code_review_models::GetReviewAssistantSettingsRequest { repo_path: repo },
            &config,
        )
        .expect("read")
        .settings,
        Some(settings)
    );
}

/// A call made while another holds the database for longer than any retry schedule would wait
/// starts once that one finishes, instead of failing.
#[test]
fn a_call_waits_for_a_long_operation_to_finish_instead_of_failing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = initialized_test_repository(temp.path());
    let repo_root = resolve_repo_root(&repo).expect("repo root");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let hold = Duration::from_secs(4);
    let (started, holding) = std::sync::mpsc::channel();
    let long = {
        let config = config.clone();
        std::thread::spawn(move || {
            with_database(&repo_root, &config, move |_database| {
                Box::pin(async move {
                    started.send(()).expect("signal");
                    tokio::time::sleep(hold).await;
                    Ok(())
                })
            })
        })
    };
    holding.recv().expect("long operation started");
    let waiting = std::time::Instant::now();
    let listed = list_review_workspaces_for_request(
        &ListReviewWorkspacesRequest {
            repo_path: repo,
            include_archived: false,
        },
        &config,
    );
    let waited = waiting.elapsed();
    long.join()
        .expect("long operation")
        .expect("long operation succeeds");
    assert!(listed.is_ok(), "{listed:?}");
    assert!(waited >= Duration::from_millis(3_500), "waited {waited:?}");
}

/// The stacks of a stack of `height` layers stepped through one layer at a time.
fn stepped_stacks(height: u64) -> Vec<bcode_code_review_models::ResolvedReviewStack> {
    (1..=height)
        .map(|top| bcode_code_review_models::ResolvedReviewStack {
            source_id: format!("source-{top}"),
            trunk: "main".to_owned(),
            trunk_oid: "0".repeat(40),
            base_oid: "0".repeat(40),
            layers: (1..=top)
                .map(|number| bcode_code_review_models::ResolvedStackLayer {
                    number,
                    title: format!("Layer {number} with a descriptive pull request title"),
                    base_oid: format!("{:040}", number - 1),
                    head_oid: format!("{number:040}"),
                    base_branch: format!("feature-branch-{}", number - 1),
                    head_branch: format!("feature-branch-{number}"),
                })
                .collect(),
        })
        .collect()
}

/// The stored JSON of `session`'s binding.
fn stored_binding_json(repo: &Path, config: &CodeReviewConfig, session: &str) -> Option<String> {
    let root = resolve_repo_root(repo).expect("root");
    let session = session.to_owned();
    with_database(&root, config, move |database| {
        Box::pin(async move {
            Ok::<_, StoreError>(
                database
                    .select("review_assistant_sessions")
                    .columns(&["binding_json"])
                    .filter(Box::new(where_eq("session_id", session)))
                    .execute_first(database)
                    .await?
                    .as_ref()
                    .and_then(|row| optional_text(row, "binding_json")),
            )
        })
    })
    .expect("read row")
}

/// A saved review with viewed state, as the review app binds it.
fn viewed_workspace(repo: &Path, id: &str) -> ReviewWorkspace {
    let mut workspace = test_workspace(repo, Vec::new());
    workspace.id = id.to_owned();
    workspace.title = "Stacked review".to_owned();
    workspace.viewed_files = (0..500)
        .map(|index| format!("src/file-{index}.rs"))
        .collect();
    workspace
}

#[test]
fn rebinding_keeps_a_stacked_review_binding_bounded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_test(&repo, &["init", "--quiet"]);
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let bind = |session: &str, height: u64, detached: bool| {
        let stacks = stepped_stacks(height);
        bind_assistant_for_request(
            BindReviewAssistantRequest {
                repo_path: repo.clone(),
                binding: ReviewAssistantBinding {
                    schema_version:
                        bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                    session_id: session.to_owned(),
                    workspace: viewed_workspace(&repo, "rv-stack"),
                    resolved: Vec::new(),
                    stacks: bcode_code_review_models::ReviewAssistantStacks::from_stacks(&stacks),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    started: None,
                    detached_at_ms: detached.then_some(1),
                    guide_grant: None,
                },
            },
            &config,
        )
        .expect("bind")
    };

    bind("s-1", 100, false);
    let first = stored_binding_json(&repo, &config, "s-1").expect("row");
    for round in 0..40 {
        bind("s-1", 100, round % 3 == 0);
    }
    bind("s-1", 100, false);
    let last = stored_binding_json(&repo, &config, "s-1").expect("row");
    assert!(
        last.len() <= first.len() + 64,
        "rebinding grew the binding from {} to {} bytes",
        first.len(),
        last.len()
    );

    // Four times the layers take about four times the room, not sixteen.
    bind("s-2", 400, false);
    let taller = stored_binding_json(&repo, &config, "s-2").expect("row");
    assert!(
        taller.len() < first.len() * 5,
        "{} bytes against {}",
        taller.len(),
        first.len()
    );

    // What the tools read is what was bound; the saved review's viewed state is read again by
    // id rather than recorded.
    let binding = assistant_binding_for_request(
        &GetReviewAssistantRequest {
            repo_path: repo.clone(),
            lookup: ReviewAssistantLookup::Session {
                session_id: "s-1".to_owned(),
            },
        },
        &config,
    )
    .expect("get")
    .binding
    .expect("bound");
    assert_eq!(binding.stacks.to_stacks(), stepped_stacks(100));
    assert_eq!(binding.workspace.id, "rv-stack");
    assert_eq!(binding.workspace.title, "Stacked review");
    assert!(binding.workspace.viewed_files.is_empty());
    assert_eq!(
        binding.started.and_then(|started| started.workspace_id),
        Some("rv-stack".to_owned())
    );
}

#[test]
fn bloated_assistant_rows_are_compacted_by_migration_with_their_meaning_kept() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_test(&repo, &["init", "--quiet"]);
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    };
    let workspace = viewed_workspace(&repo, "rv-stack");
    let stacks = stepped_stacks(60);
    let transient = crate::scope::transient_workspace(repo.clone(), None, Vec::new());
    let rows = legacy_assistant_rows(&workspace, &transient, &stacks);
    let before: BTreeMap<&str, String> = rows.iter().cloned().collect();
    insert_and_compact(&repo, &config, rows);

    let get = |session: &str| {
        assistant_binding_for_request(
            &GetReviewAssistantRequest {
                repo_path: repo.clone(),
                lookup: ReviewAssistantLookup::Session {
                    session_id: session.to_owned(),
                },
            },
            &config,
        )
        .expect("get")
        .binding
    };
    for session in ["tagged", "untagged-detached", "transient"] {
        let json = stored_binding_json(&repo, &config, session).expect("row");
        assert!(
            json.len() * 4 < before[session].len(),
            "{session}: {} bytes, {} before",
            json.len(),
            before[session].len()
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert!(value.get("stacks").is_none(), "{session}: {json}");
        let binding = get(session).expect("readable");
        assert_eq!(binding.stacks.to_stacks(), stacks, "{session}");
    }

    let tagged = get("tagged").expect("tagged");
    assert_eq!(
        tagged.started,
        Some(ReviewAssistantTag {
            workspace_id: Some("rv-stack".to_owned()),
            title: Some("Stacked review".to_owned()),
            layer: Some(7),
        })
    );
    assert_eq!((tagged.created_at_ms, tagged.updated_at_ms), (5, 9));
    assert_eq!(tagged.workspace.id, "rv-stack");
    assert_eq!(tagged.workspace.sources, workspace.sources);
    assert!(tagged.workspace.viewed_files.is_empty());

    let untagged = get("untagged-detached").expect("untagged");
    assert_eq!(untagged.started, None);
    assert_eq!(untagged.detached_at_ms, Some(7));
    assert_eq!(untagged.workspace.title, "Stacked review");

    let transient_binding = get("transient").expect("transient");
    assert_eq!(transient_binding.workspace, transient);

    for session in ["future", "damaged"] {
        assert_eq!(
            stored_binding_json(&repo, &config, session).as_deref(),
            Some(before[session].as_str()),
            "{session} is left as it was"
        );
    }
}

/// Assistant rows as earlier versions wrote them: every stack in full and the saved review's
/// viewed state, with and without a start tag, attached and detached; a transient review; and
/// rows this version cannot read.
fn legacy_assistant_rows(
    workspace: &ReviewWorkspace,
    transient: &ReviewWorkspace,
    stacks: &[bcode_code_review_models::ResolvedReviewStack],
) -> Vec<(&'static str, String)> {
    vec![
        (
            "tagged",
            serde_json::json!({
                "schema_version": bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                "session_id": "tagged",
                "workspace": workspace,
                "stacks": stacks,
                "created_at_ms": 5,
                "updated_at_ms": 9,
                "started": { "workspace_id": "rv-stack", "title": "Stacked review", "layer": 7 },
            })
            .to_string(),
        ),
        (
            "untagged-detached",
            serde_json::json!({
                "schema_version": bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                "session_id": "untagged-detached",
                "workspace": workspace,
                "stacks": stacks,
                "created_at_ms": 6,
                "updated_at_ms": 8,
                "detached_at_ms": 7,
            })
            .to_string(),
        ),
        (
            "transient",
            serde_json::json!({
                "schema_version": bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                "session_id": "transient",
                "workspace": transient,
                "stacks": stacks,
                "created_at_ms": 1,
                "updated_at_ms": 1,
            })
            .to_string(),
        ),
        (
            "future",
            serde_json::json!({
                "schema_version": 99,
                "session_id": "future",
                "workspace": workspace,
                "stacks": stacks,
                "created_at_ms": 1,
                "updated_at_ms": 1,
            })
            .to_string(),
        ),
        ("damaged", "{not json".to_owned()),
    ]
}

/// Insert `rows` as assistant bindings and run the binding compaction again, as on a database
/// written before it existed.
fn insert_and_compact(repo: &Path, config: &CodeReviewConfig, rows: Vec<(&'static str, String)>) {
    let root = resolve_repo_root(repo).expect("root");
    let db_root = root.clone();
    with_database(&root, config, move |database| {
        Box::pin(async move {
            for (session, json) in rows {
                database
                    .insert("review_assistant_sessions")
                    .value("session_id", session.to_owned())
                    .value("workspace_id", "rv-stack".to_owned())
                    .value("repo_root", db_root.display().to_string())
                    .value("binding_json", json)
                    .value("created_at_ms", 1_i64)
                    .value("updated_at_ms", 1_i64)
                    .execute(database)
                    .await?;
            }
            database
                .delete(MIGRATIONS_TABLE)
                .filter(Box::new(where_eq(
                    "id",
                    "030_compact_review_assistant_bindings",
                )))
                .execute(database)
                .await?;
            run_migrations(database).await
        })
    })
    .expect("migrate");
}
