//! Several processes share the review state and cache databases: review apps in different
//! repositories, review apps in the same repository, and the daemon's code review plugin. Every
//! operation of every process succeeds while the others keep reading and writing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bcode_code_review::cache::{CacheKind, ReviewCache};
use bcode_code_review::{CodeReviewConfig, CodeReviewStateLocation, ReviewStore};
use bcode_code_review_models::{
    BindReviewAssistantRequest, CreateReviewWorkspaceRequest, ListReviewAssistantsRequest,
    ListReviewWorkspacesRequest, REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ReviewAssistantBinding,
    ReviewTarget, ReviewWorkspace, UpdateReviewWorkspaceRequest,
};

/// Names the worker role of a re-executed test binary.
const WORKER_ENV: &str = "BCODE_CODE_REVIEW_CONCURRENCY_WORKER";
const WORKER_REPO_ENV: &str = "BCODE_CODE_REVIEW_CONCURRENCY_REPO";
const WORKER_STATE_ENV: &str = "BCODE_CODE_REVIEW_CONCURRENCY_STATE";
const WORKER_SECONDS_ENV: &str = "BCODE_CODE_REVIEW_CONCURRENCY_SECONDS";
const TEST_NAME: &str = "processes_share_the_state_database_without_failing";
/// Size of the assistant bindings the workers write, near what a stacked review records.
const LARGE_BINDING_BYTES: usize = 1_500_000;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

fn repository(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet"]);
    git(path, &["config", "user.email", "review@example.test"]);
    git(path, &["config", "user.name", "Review Test"]);
    std::fs::write(path.join("file.txt"), "base\n").unwrap();
    git(path, &["add", "file.txt"]);
    git(path, &["commit", "--quiet", "-m", "base"]);
    path.canonicalize().unwrap()
}

fn store(state: &Path) -> ReviewStore {
    ReviewStore::new(CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(state.to_path_buf()),
    })
}

/// What a review app or the plugin does: list and open reviews, mark files viewed, and bind
/// (or refresh) an assistant session with a large recorded review.
fn record(failures: &mut Vec<String>, result: Result<(), String>) {
    if let Err(error) = result {
        failures.push(error);
    }
}

fn worker(role: &str, repo: &Path, state: &Path, run_for: Duration) -> Vec<String> {
    let store = store(state);
    let mut failures = Vec::new();
    let workspace = loop {
        match store.create_workspace(CreateReviewWorkspaceRequest {
            repo_path: repo.to_path_buf(),
            title: Some(format!("{role} review")),
            sources: ReviewWorkspace::from_target(repo.to_path_buf(), ReviewTarget::LastCommit)
                .sources,
            presentation: None,
        }) {
            Ok(response) => break response.workspace,
            Err(error) => {
                failures.push(format!("create: {error}"));
                if failures.len() > 50 {
                    return failures;
                }
            }
        }
    };
    let started = Instant::now();
    let mut round = 0_u64;
    while started.elapsed() < run_for {
        round += 1;
        record(
            &mut failures,
            store
                .list_workspaces(&ListReviewWorkspacesRequest {
                    repo_path: repo.to_path_buf(),
                    include_archived: false,
                })
                .map(drop)
                .map_err(|error| format!("list: {error}")),
        );
        let mut viewed = workspace.clone();
        viewed.viewed_files.insert(format!("{role}-{round}.txt"));
        record(
            &mut failures,
            store
                .update_workspace(UpdateReviewWorkspaceRequest {
                    repo_path: repo.to_path_buf(),
                    workspace: viewed,
                    base: Some(workspace.clone()),
                })
                .map(drop)
                .map_err(|error| format!("mark viewed: {error}")),
        );
        let mut recorded = workspace.clone();
        recorded.title = "x".repeat(LARGE_BINDING_BYTES);
        record(
            &mut failures,
            store
                .bind_assistant(BindReviewAssistantRequest {
                    repo_path: repo.to_path_buf(),
                    binding: ReviewAssistantBinding {
                        schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                        session_id: format!("{role}-session"),
                        workspace: recorded,
                        resolved: Vec::new(),
                        stacks: Vec::new(),
                        created_at_ms: 0,
                        updated_at_ms: 0,
                        started: None,
                        detached_at_ms: None,
                    },
                })
                .map(drop)
                .map_err(|error| format!("bind: {error}")),
        );
        record(
            &mut failures,
            store
                .list_assistants(&ListReviewAssistantsRequest {
                    repo_path: repo.to_path_buf(),
                    limit: None,
                })
                .map(drop)
                .map_err(|error| format!("list assistants: {error}")),
        );
    }
    eprintln!("{role}: {round} rounds, {} failures", failures.len());
    failures
}

#[test]
fn processes_share_the_state_database_without_failing() {
    if let Ok(role) = std::env::var(WORKER_ENV) {
        let repo = PathBuf::from(std::env::var(WORKER_REPO_ENV).unwrap());
        let state = PathBuf::from(std::env::var(WORKER_STATE_ENV).unwrap());
        let seconds = std::env::var(WORKER_SECONDS_ENV).unwrap().parse().unwrap();
        // Each process also overlaps its own calls, as the app's blocking pool does.
        let threads: Vec<_> = (0..2)
            .map(|thread| {
                let role = format!("{role}.{thread}");
                let repo = repo.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    worker(&role, &repo, &state, Duration::from_secs(seconds))
                })
            })
            .collect();
        let failures: Vec<String> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        for failure in failures.iter().take(5) {
            eprintln!("{role} failure: {failure}");
        }
        assert!(failures.is_empty(), "{role}: {} failures", failures.len());
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let first = repository(&temp.path().join("first"));
    let second = repository(&temp.path().join("second"));
    let third = repository(&temp.path().join("third"));
    let seconds = 6;
    // Two apps in different repositories, a second app in the first repository, and a
    // process standing for the daemon's plugin.
    let roles = [
        ("app-first", &first),
        ("app-second", &second),
        ("app-first-again", &first),
        ("plugin", &third),
    ];
    let children: Vec<_> = roles
        .iter()
        .map(|(role, repo)| {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads", "1"])
                .env(WORKER_ENV, role)
                .env(WORKER_REPO_ENV, repo.as_os_str())
                .env(WORKER_STATE_ENV, &state)
                .env(WORKER_SECONDS_ENV, seconds.to_string())
                .spawn()
                .unwrap();
            (*role, child)
        })
        .collect();
    let failed: Vec<&str> = children
        .into_iter()
        .filter_map(|(role, mut child)| (!child.wait().unwrap().success()).then_some(role))
        .collect();
    assert!(failed.is_empty(), "workers failed: {failed:?}");
}

const CACHE_WORKER_ENV: &str = "BCODE_CODE_REVIEW_CACHE_WORKER";
const CACHE_TEST_NAME: &str = "a_second_process_shares_the_cache_another_has_open";

#[test]
fn a_second_process_shares_the_cache_another_has_open() {
    if let Ok(path) = std::env::var(CACHE_WORKER_ENV) {
        let cache = ReviewCache::open(Path::new(&path));
        assert!(cache.enabled(), "the second process has a cache");
        let found = cache.get_many(CacheKind::MergeBase, &["first".to_owned()]);
        assert_eq!(
            found.get("first").map(|entry| entry.value.as_str()),
            Some("from the first process")
        );
        cache.put_many(
            CacheKind::MergeBase,
            &[("second".to_owned(), "from the second process".to_owned())],
        );
        cache.close();
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("review-cache.db");
    // The first app keeps its cache for as long as it runs.
    let cache = ReviewCache::open(&path);
    assert!(cache.enabled());
    cache.put_many(
        CacheKind::MergeBase,
        &[("first".to_owned(), "from the first process".to_owned())],
    );
    // Long enough for the held write to be written.
    std::thread::sleep(Duration::from_secs(1));
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            CACHE_TEST_NAME,
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CACHE_WORKER_ENV, &path)
        .status()
        .unwrap();
    assert!(status.success(), "the second process failed");
    let found = cache.get_many(CacheKind::MergeBase, &["second".to_owned()]);
    assert_eq!(
        found.get("second").map(|entry| entry.value.as_str()),
        Some("from the second process")
    );
}
