//! Repository read tests against generated fixture repositories.

use super::{CatFileRegistry, DataError, ReviewData, index};
use crate::testing::fixture::{
    BASE_BRANCH, BINARY_FILE, CRLF_FILE, FixtureSpec, GitFixture, HEAD_BRANCH, LARGE_FILE,
    MODE_CHANGE_FILE, STACK_BRANCHES, STACK_SHARED_FILE, SUBMODULE_COMMIT, SUBMODULE_PATH,
    SYMLINK_FILE, SYMLINK_TARGET, UNICODE_FILE, UNTRACKED_FILE,
};
use crate::viewed::{
    EntryIdentity, ViewedIdentity, ViewedState, effective_marks, mark_viewed, viewed_state,
};
use bcode_code_review_models::{
    REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED, REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION,
    REVIEW_DIAGNOSTIC_UNSUPPORTED_SOURCE, REVIEW_ERROR_PATH_REJECTED, ResolveReviewSourcesRequest,
    ResolvedSource, ResolvedSourceMode, ReviewBlobLinesRequest, ReviewBlobLinesResponse,
    ReviewBlobRef, ReviewBlobSelector, ReviewCommitsPageRequest, ReviewFileHunksRequest,
    ReviewFileHunksResponse, ReviewFileStatus, ReviewFilesSearchRequest, ReviewIndexEntry,
    ReviewIndexFlag, ReviewIndexRequest, ReviewLineKind, ReviewRefKind, ReviewRefsSearchRequest,
    ReviewRepoStateRequest, ReviewSource, ReviewSourceKind, ReviewSourcesTokenRequest, RevisionRef,
};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn source(id: &str, kind: ReviewSourceKind) -> ReviewSource {
    ReviewSource {
        id: id.to_string(),
        label: kind.label(),
        kind,
        included: true,
    }
}

fn resolve_one(service: &mut ReviewData, repo: &Path, kind: ReviewSourceKind) -> ResolvedSource {
    let response = service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![source("s", kind)],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve");
    assert!(
        response.diagnostics.is_empty(),
        "{:?}",
        response.diagnostics
    );
    assert_eq!(response.sources.len(), 1, "{:?}", response.sources);
    response.sources.into_iter().next().expect("one source")
}

fn branch_compare() -> ReviewSourceKind {
    ReviewSourceKind::BranchCompare {
        base_branch: BASE_BRANCH.to_string(),
        head_branch: HEAD_BRANCH.to_string(),
        merge_base: true,
    }
}

fn compare(base: &str, head: &str) -> ReviewSourceKind {
    ReviewSourceKind::CommitRange {
        base: base.to_string(),
        head: head.to_string(),
        merge_base: false,
    }
}

fn full_index(
    service: &mut ReviewData,
    repo: &Path,
    source: &ResolvedSource,
) -> Vec<ReviewIndexEntry> {
    service
        .index_get(&ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source: source.clone(),
            cursor: None,
            limit: Some(5_000),
        })
        .expect("index")
        .entries
}

fn entry<'a>(entries: &'a [ReviewIndexEntry], path: &str) -> &'a ReviewIndexEntry {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("{path} missing from index"))
}

fn rev_parse(fixture: &GitFixture, spec: &str) -> String {
    fixture.git(&["rev-parse", spec]).trim().to_string()
}

fn hunks_request(repo: &Path, source: &ResolvedSource, path: &str) -> ReviewFileHunksRequest {
    ReviewFileHunksRequest {
        repo_path: repo.to_path_buf(),
        source: source.clone(),
        path: path.to_string(),
        old_path: None,
        context_lines: None,
        ignore_whitespace: false,
        max_lines: None,
    }
}

fn hunks(service: &mut ReviewData, request: &ReviewFileHunksRequest) -> ReviewFileHunksResponse {
    service.file_hunks_get(request).expect("hunks")
}

fn blob_lines(
    service: &mut ReviewData,
    repo: &Path,
    blob: ReviewBlobSelector,
    start_line: u32,
    count: u32,
) -> Result<ReviewBlobLinesResponse, DataError> {
    service.blob_lines_get(&ReviewBlobLinesRequest {
        repo_path: repo.to_path_buf(),
        blob,
        start_line,
        count,
    })
}

fn object(oid: &str) -> ReviewBlobSelector {
    ReviewBlobSelector::Object {
        oid: oid.to_string(),
    }
}

fn at(revision: RevisionRef, path: &str) -> ReviewBlobSelector {
    ReviewBlobSelector::Revision {
        revision,
        path: path.to_string(),
    }
}

fn added_lines(response: &ReviewFileHunksResponse) -> Vec<String> {
    response
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == ReviewLineKind::Added)
        .map(|line| line.content.clone())
        .collect()
}

// Source resolution -----------------------------------------------------------------------------

#[test]
fn working_tree_and_index_sources_resolve_to_live_sides() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let head = RevisionRef::commit(rev_parse(&fixture, "HEAD"));

    let unstaged = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeUnstaged);
    assert_eq!(
        (unstaged.base, unstaged.head),
        (RevisionRef::Index, RevisionRef::WorkingTree)
    );
    let staged = resolve_one(&mut service, repo, ReviewSourceKind::IndexStaged);
    assert_eq!(
        (staged.base, staged.head),
        (head.clone(), RevisionRef::Index)
    );
    let both = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeAndIndex);
    assert_eq!((both.base, both.head), (head, RevisionRef::WorkingTree));
}

#[test]
fn commit_sources_resolve_to_pinned_oids() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let base = rev_parse(&fixture, BASE_BRANCH);
    let head = rev_parse(&fixture, HEAD_BRANCH);

    let last = resolve_one(&mut service, repo, ReviewSourceKind::LastCommit);
    assert_eq!(last.base, RevisionRef::commit(base.clone()));
    assert_eq!(last.head, RevisionRef::commit(head.clone()));

    let root_commit = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::Commit {
            rev: BASE_BRANCH.to_string(),
        },
    );
    assert_eq!(
        root_commit.base,
        RevisionRef::EmptyTree,
        "root commit diffs the empty tree"
    );

    let range = resolve_one(&mut service, repo, compare(BASE_BRANCH, STACK_BRANCHES[2]));
    assert_eq!(range.base, RevisionRef::commit(base.clone()));
    assert_eq!(
        range.head,
        RevisionRef::commit(rev_parse(&fixture, STACK_BRANCHES[2]))
    );

    // Merge-base semantics: extra-0 sits at the base commit, so main...extra-0 starts there.
    let branch = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::BranchCompare {
            base_branch: HEAD_BRANCH.to_string(),
            head_branch: "fixture/extra-0".to_string(),
            merge_base: true,
        },
    );
    assert_eq!(branch.base, RevisionRef::commit(base.clone()));
    assert_eq!(branch.head, RevisionRef::commit(base));

    let current = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::CurrentBranch {
            base: Some(BASE_BRANCH.to_string()),
        },
    );
    assert_eq!(current.head, RevisionRef::commit(head));
}

#[test]
fn current_branch_detects_origin_head_as_base() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    fixture.run(&["update-ref", "refs/remotes/origin/main", BASE_BRANCH]);
    fixture.run(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    let current = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::CurrentBranch { base: None },
    );
    assert_eq!(
        current.base,
        RevisionRef::commit(rev_parse(&fixture, BASE_BRANCH))
    );
    assert_eq!(
        current.head,
        RevisionRef::commit(rev_parse(&fixture, "HEAD"))
    );
}

#[test]
fn repo_state_reports_branch_base_ahead_and_change_counts() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    fixture.run(&["update-ref", "refs/remotes/origin/main", BASE_BRANCH]);
    fixture.run(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    let state = service
        .repo_state(&ReviewRepoStateRequest {
            repo_path: repo.to_path_buf(),
        })
        .expect("repo state");
    assert_eq!(state.branch.as_deref(), Some(HEAD_BRANCH));
    assert_eq!(state.head_oid, Some(rev_parse(&fixture, "HEAD")));
    assert_eq!(state.base.as_deref(), Some("origin/main"));
    assert_eq!(state.merge_base_oid, Some(rev_parse(&fixture, BASE_BRANCH)));
    assert_eq!(state.ahead, Some(1));
    let count = |args: &[&str]| fixture.git(args).lines().count();
    let unstaged = count(&["diff", "--name-only"]);
    let staged = count(&["diff", "--cached", "--name-only"]);
    assert!(unstaged > 0 && staged > 0);
    assert_eq!(state.unstaged.files as usize, unstaged);
    assert_eq!(state.staged.files as usize, staged);
    assert!(state.unstaged.additions > 0 && state.unstaged.deletions > 0);

    // A detached head has no branch; a clean tree has no changes.
    fixture.run(&["reset", "--hard", "-q"]);
    fixture.run(&["checkout", "-q", "--detach", "HEAD"]);
    let state = service
        .repo_state(&ReviewRepoStateRequest {
            repo_path: repo.to_path_buf(),
        })
        .expect("repo state");
    assert_eq!(state.branch, None);
    assert!(state.unstaged.is_empty() && state.staged.is_empty());
}

#[test]
fn commit_selection_resolves_to_one_range_or_parts() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();

    let contiguous = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::Commits {
            revs: vec![STACK_BRANCHES[1].to_string(), STACK_BRANCHES[0].to_string()],
        },
    );
    assert_eq!(
        contiguous.base,
        RevisionRef::commit(rev_parse(&fixture, HEAD_BRANCH))
    );
    assert_eq!(
        contiguous.head,
        RevisionRef::commit(rev_parse(&fixture, STACK_BRANCHES[1]))
    );
    assert!(contiguous.part.is_none());

    let response = service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![source(
                "s",
                ReviewSourceKind::Commits {
                    revs: vec![STACK_BRANCHES[2].to_string(), STACK_BRANCHES[0].to_string()],
                },
            )],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve");
    let parts: Vec<_> = response
        .sources
        .iter()
        .map(|source| {
            let part = source.part.as_ref().expect("part");
            (part.index, part.count, source.head.clone())
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            (
                0,
                2,
                RevisionRef::commit(rev_parse(&fixture, STACK_BRANCHES[2]))
            ),
            (
                1,
                2,
                RevisionRef::commit(rev_parse(&fixture, STACK_BRANCHES[0]))
            ),
        ]
    );
    assert_eq!(
        response.sources[0].base,
        RevisionRef::commit(rev_parse(&fixture, STACK_BRANCHES[1]))
    );
    assert!(
        response.sources[0]
            .part
            .as_ref()
            .is_some_and(|part| part.label.contains("stack layer 3"))
    );
}

#[test]
fn provider_unsupported_and_missing_sources_produce_diagnostics_without_failing_others() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let unsupported: ReviewSourceKind =
        serde_json::from_str(r#"{"kind":"from_the_future","x":1}"#).expect("unknown kind");
    let mut excluded = source("excluded", ReviewSourceKind::LastCommit);
    excluded.included = false;
    let provider = |number| ("github".to_string(), "o/r".to_string(), number);
    let (pr_provider, pr_repo, pr_number) = provider(1);
    let response = service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![
                source(
                    "pr",
                    ReviewSourceKind::PullRequest {
                        provider: pr_provider,
                        repository: pr_repo,
                        number: pr_number,
                    },
                ),
                source(
                    "layer",
                    ReviewSourceKind::StackLayer {
                        provider: "github".into(),
                        repository: "o/r".into(),
                        number: 2,
                    },
                ),
                source(
                    "stack",
                    ReviewSourceKind::Stack {
                        provider: "github".into(),
                        repository: "o/r".into(),
                        top: 3,
                    },
                ),
                source("future", unsupported),
                source(
                    "missing",
                    ReviewSourceKind::BranchCompare {
                        base_branch: "no-such-branch".into(),
                        head_branch: HEAD_BRANCH.into(),
                        merge_base: true,
                    },
                ),
                source(
                    "bad",
                    ReviewSourceKind::Commit {
                        rev: "--output=/tmp/pwned".into(),
                    },
                ),
                source(
                    "escape",
                    ReviewSourceKind::File {
                        path: "../outside.txt".into(),
                    },
                ),
                excluded,
                source("ok", ReviewSourceKind::LastCommit),
            ],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve");
    let codes: Vec<(&str, &str)> = response
        .diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.source_id.as_str(), diagnostic.code.as_str()))
        .collect();
    assert_eq!(
        codes,
        vec![
            ("pr", REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED),
            ("layer", REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED),
            ("stack", REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED),
            ("future", REVIEW_DIAGNOSTIC_UNSUPPORTED_SOURCE),
            ("missing", REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION),
            ("bad", REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION),
            ("escape", REVIEW_ERROR_PATH_REJECTED),
        ]
    );
    let resolved: Vec<&str> = response
        .sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect();
    assert_eq!(resolved, vec!["ok"]);
}

#[test]
fn browse_sources_list_tracked_files() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let repository = resolve_one(&mut service, repo, ReviewSourceKind::Repository);
    assert_eq!(repository.mode, ResolvedSourceMode::Browse);
    let entries = full_index(&mut service, repo, &repository);
    assert!(entries.iter().any(|entry| entry.path == LARGE_FILE));
    assert!(entry(&entries, SYMLINK_FILE).has(ReviewIndexFlag::Symlink));
    assert!(entry(&entries, SUBMODULE_PATH).has(ReviewIndexFlag::Submodule));

    let file = resolve_one(
        &mut service,
        repo,
        ReviewSourceKind::FileRange {
            path: UNICODE_FILE.to_string(),
            start: 9,
            end: 2,
        },
    );
    assert_eq!(
        file.line_range.map(|range| (range.start, range.end)),
        Some((2, 9))
    );
    let entries = full_index(&mut service, repo, &file);
    assert_eq!(entries.len(), 1);
    assert!(
        service
            .file_hunks_get(&hunks_request(repo, &file, UNICODE_FILE))
            .is_err()
    );
}

// Index -----------------------------------------------------------------------------------------

#[test]
fn branch_compare_index_reports_every_change_kind() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = resolve_one(&mut service, repo, branch_compare());
    let entries = full_index(&mut service, repo, &source);

    let modified = entry(&entries, &GitFixture::modified_path(0));
    assert_eq!(modified.status, ReviewFileStatus::Modified);
    assert_eq!(
        modified.old_blob,
        ReviewBlobRef::Object {
            oid: rev_parse(&fixture, &format!("{BASE_BRANCH}:{}", modified.path))
        }
    );
    assert_eq!(
        modified.new_blob,
        ReviewBlobRef::Object {
            oid: rev_parse(&fixture, &format!("{HEAD_BRANCH}:{}", modified.path))
        }
    );
    assert!(modified.additions > 0 && modified.additions == modified.deletions);

    let (old, new) = GitFixture::renamed_paths(0);
    let renamed = entry(&entries, &new);
    assert_eq!(renamed.status, ReviewFileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some(old.as_str()));
    assert!(renamed.similarity.is_some_and(|score| score > 50));
    assert_eq!((renamed.additions, renamed.deletions), (1, 1));

    let deleted = entry(&entries, &GitFixture::deleted_path(0));
    assert_eq!(deleted.status, ReviewFileStatus::Deleted);
    assert_eq!(deleted.new_blob, ReviewBlobRef::Absent);
    assert!(deleted.new_mode.is_none());

    let added = entry(&entries, &GitFixture::added_path(0));
    assert_eq!(added.status, ReviewFileStatus::Added);
    assert_eq!(added.old_blob, ReviewBlobRef::Absent);

    let binary = entry(&entries, BINARY_FILE);
    assert!(binary.has(ReviewIndexFlag::Binary) && binary.additions == 0);

    let mode = entry(&entries, MODE_CHANGE_FILE);
    assert!(mode.has(ReviewIndexFlag::ModeChanged));
    assert_eq!((mode.additions, mode.deletions), (0, 0));
    assert_eq!(mode.new_mode.as_deref(), Some("100755"));

    assert!(entry(&entries, SYMLINK_FILE).has(ReviewIndexFlag::Symlink));
    let submodule = entry(&entries, SUBMODULE_PATH);
    assert!(submodule.has(ReviewIndexFlag::Submodule));
    assert_eq!(
        submodule.new_blob,
        ReviewBlobRef::Object {
            oid: SUBMODULE_COMMIT.to_string()
        }
    );
    assert!(entry(&entries, UNICODE_FILE).additions > 0);
    assert_eq!(entry(&entries, CRLF_FILE).additions, 1);

    // Totals agree with git's own numstat.
    let numstat = fixture.git(&[
        "diff",
        "--numstat",
        "-M",
        &format!("{BASE_BRANCH}...{HEAD_BRANCH}"),
    ]);
    let (additions, deletions) = numstat.lines().fold((0_u64, 0_u64), |(add, del), line| {
        let mut fields = line.split('\t');
        let added = fields
            .next()
            .and_then(|field| field.parse::<u64>().ok())
            .unwrap_or(0);
        let deleted = fields
            .next()
            .and_then(|field| field.parse::<u64>().ok())
            .unwrap_or(0);
        (add + added, del + deleted)
    });
    let response = service
        .index_get(&ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source,
            cursor: None,
            limit: None,
        })
        .expect("index");
    assert_eq!(
        (response.total_additions, response.total_deletions),
        (additions, deletions)
    );
    assert_eq!(response.total_entries as usize, numstat.lines().count());
}

#[test]
fn staged_unstaged_and_combined_indexes_use_index_and_working_tree_blobs() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let mixed = GitFixture::modified_path(0);
    let unstaged_only = GitFixture::modified_path(1);
    let index_oid = |path: &str| rev_parse(&fixture, &format!(":{path}"));
    let head_oid = |path: &str| rev_parse(&fixture, &format!("HEAD:{path}"));

    let unstaged = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeUnstaged);
    let entries = full_index(&mut service, repo, &unstaged);
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, vec![mixed.as_str(), unstaged_only.as_str()]);
    assert_eq!(
        entries[0].old_blob,
        ReviewBlobRef::Object {
            oid: index_oid(&mixed)
        }
    );
    assert_eq!(entries[0].new_blob, ReviewBlobRef::WorkingTree);

    let staged = resolve_one(&mut service, repo, ReviewSourceKind::IndexStaged);
    let entries = full_index(&mut service, repo, &staged);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (&entries[0].old_blob, &entries[0].new_blob),
        (
            &ReviewBlobRef::Object {
                oid: head_oid(&mixed)
            },
            &ReviewBlobRef::Object {
                oid: index_oid(&mixed)
            }
        )
    );

    let both = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeAndIndex);
    let entries = full_index(&mut service, repo, &both);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].old_blob,
        ReviewBlobRef::Object {
            oid: head_oid(&mixed)
        }
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.new_blob == ReviewBlobRef::WorkingTree)
    );
}

#[test]
fn index_pages_concatenate_to_the_full_listing_and_reject_stale_cursors() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = resolve_one(&mut service, repo, branch_compare());
    let all = full_index(&mut service, repo, &source);
    assert!(all.len() > 6);

    let mut paged = Vec::new();
    let mut cursor = None;
    let mut page_count = 0;
    loop {
        let response = service
            .index_get(&ReviewIndexRequest {
                repo_path: repo.to_path_buf(),
                source: source.clone(),
                cursor: cursor.clone(),
                limit: Some(3),
            })
            .expect("page");
        assert!(response.entries.len() <= 3);
        paged.extend(response.entries);
        page_count += 1;
        cursor = response.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(paged, all);
    assert_eq!(page_count, all.len().div_ceil(3));
    assert_eq!(
        index::cached_listing_count(&service.indexes),
        1,
        "immutable listing cached once"
    );

    // A live source whose content changes between pages invalidates its cursor.
    let unstaged = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeUnstaged);
    let first = service
        .index_get(&ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source: unstaged.clone(),
            cursor: None,
            limit: Some(1),
        })
        .expect("first page");
    std::fs::write(repo.join(GitFixture::modified_path(2)), "rewritten\n").expect("edit");
    let stale = service.index_get(&ReviewIndexRequest {
        repo_path: repo.to_path_buf(),
        source: unstaged.clone(),
        cursor: first.next_cursor,
        limit: Some(1),
    });
    assert!(matches!(stale, Err(DataError::CursorStale)), "{stale:?}");
    let malformed = service.index_get(&ReviewIndexRequest {
        repo_path: repo.to_path_buf(),
        source: unstaged,
        cursor: Some("not-a-cursor".to_string()),
        limit: None,
    });
    assert!(matches!(malformed, Err(DataError::InvalidRequest(_))));
}

// Hunks -----------------------------------------------------------------------------------------

#[test]
fn modified_file_hunks_match_both_blob_sides() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = resolve_one(&mut service, repo, branch_compare());
    let entries = full_index(&mut service, repo, &source);
    let file = entry(&entries, &GitFixture::modified_path(0)).clone();
    let response = hunks(&mut service, &hunks_request(repo, &source, &file.path));
    assert!(!response.hunks.is_empty() && response.truncated.is_none());
    assert!(response.patch_id.is_some());

    let old_oid = file.old_blob.oid().expect("old oid");
    let new_oid = file.new_blob.oid().expect("new oid");
    let old = blob_lines(&mut service, repo, object(old_oid), 1, 5_000).expect("old blob");
    let new = blob_lines(&mut service, repo, object(new_oid), 1, 5_000).expect("new blob");
    let mut added = 0;
    let mut removed = 0;
    for line in response.hunks.iter().flat_map(|hunk| &hunk.lines) {
        if let Some(number) = line.old_line {
            assert_eq!(old.lines[number as usize - 1], line.content);
        }
        if let Some(number) = line.new_line {
            assert_eq!(new.lines[number as usize - 1], line.content);
        }
        match line.kind {
            ReviewLineKind::Added => added += 1,
            ReviewLineKind::Removed => removed += 1,
            ReviewLineKind::Context => {}
        }
    }
    assert_eq!((added, removed), (file.additions, file.deletions));

    let no_context = hunks(
        &mut service,
        &ReviewFileHunksRequest {
            context_lines: Some(0),
            ..hunks_request(repo, &source, &file.path)
        },
    );
    assert!(
        no_context
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .all(|line| line.kind != ReviewLineKind::Context)
    );
    assert_eq!(
        no_context.patch_id, response.patch_id,
        "context does not affect identity"
    );
}

#[test]
fn renamed_deleted_binary_unicode_crlf_and_mode_hunks() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = resolve_one(&mut service, repo, branch_compare());

    let (old, new) = GitFixture::renamed_paths(0);
    let renamed = hunks(
        &mut service,
        &ReviewFileHunksRequest {
            old_path: Some(old),
            ..hunks_request(repo, &source, &new)
        },
    );
    assert_eq!(renamed.hunks.len(), 1);

    let deleted = hunks(
        &mut service,
        &hunks_request(repo, &source, &GitFixture::deleted_path(0)),
    );
    assert_ne!(deleted.hunks, Vec::new());
    assert!(
        deleted
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .all(|line| line.kind == ReviewLineKind::Removed && line.new_line.is_none())
    );

    let binary = hunks(&mut service, &hunks_request(repo, &source, BINARY_FILE));
    assert!(binary.binary && binary.hunks.is_empty() && binary.patch_id.is_none());

    let unicode = hunks(&mut service, &hunks_request(repo, &source, UNICODE_FILE));
    assert!(
        added_lines(&unicode)
            .iter()
            .any(|line| line.contains("🦀") && line.contains("改訂"))
    );

    let crlf = hunks(&mut service, &hunks_request(repo, &source, CRLF_FILE));
    assert!(
        crlf.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .all(|line| !line.content.contains('\r'))
    );
    assert_eq!(added_lines(&crlf), vec!["middle edited line"]);

    let mode = hunks(
        &mut service,
        &hunks_request(repo, &source, MODE_CHANGE_FILE),
    );
    assert_eq!(mode.hunks, Vec::new());
    let unchanged = hunks(
        &mut service,
        &hunks_request(repo, &source, "no/such/file.rs"),
    );
    assert_ne!(
        mode.patch_id, unchanged.patch_id,
        "a mode change has its own identity"
    );

    let symlink = hunks(&mut service, &hunks_request(repo, &source, SYMLINK_FILE));
    assert_eq!(added_lines(&symlink), vec![SYMLINK_TARGET]);
}

#[test]
fn huge_file_hunks_truncate_at_the_line_budget_with_a_stable_identity() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = resolve_one(&mut service, repo, branch_compare());
    let full = hunks(&mut service, &hunks_request(repo, &source, LARGE_FILE));
    let total: usize = full.hunks.iter().map(|hunk| hunk.lines.len()).sum();
    let truncated = hunks(
        &mut service,
        &ReviewFileHunksRequest {
            max_lines: Some(50),
            ..hunks_request(repo, &source, LARGE_FILE)
        },
    );
    let marker = truncated.truncated.expect("truncation marker");
    assert_eq!(marker.shown_lines, 50);
    assert_eq!(marker.total_lines as usize, total);
    assert_eq!(marker.total_hunks as usize, full.hunks.len());
    let shown: usize = truncated.hunks.iter().map(|hunk| hunk.lines.len()).sum();
    assert_eq!(shown, 50);
    let last = truncated.hunks.last().expect("hunk");
    let old_lines = last
        .lines
        .iter()
        .filter(|line| line.old_line.is_some())
        .count();
    assert_eq!(
        last.old_count as usize, old_lines,
        "cut hunk counts describe its lines"
    );
    assert_eq!(truncated.patch_id, full.patch_id);
}

#[test]
fn staged_and_unstaged_hunks_differ_for_a_mixed_file_and_whitespace_mode_drops_identity() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let path = GitFixture::modified_path(0);
    let staged = resolve_one(&mut service, repo, ReviewSourceKind::IndexStaged);
    let unstaged = resolve_one(&mut service, repo, ReviewSourceKind::WorkingTreeUnstaged);
    let staged_hunks = hunks(&mut service, &hunks_request(repo, &staged, &path));
    let unstaged_hunks = hunks(&mut service, &hunks_request(repo, &unstaged, &path));
    assert_ne!(added_lines(&staged_hunks), Vec::<String>::new());
    assert!(
        added_lines(&staged_hunks)
            .iter()
            .all(|line| line.contains("staged"))
    );
    assert!(
        added_lines(&unstaged_hunks)
            .iter()
            .all(|line| line.contains("unstaged"))
    );
    assert_ne!(staged_hunks.patch_id, unstaged_hunks.patch_id);

    let ignoring = hunks(
        &mut service,
        &ReviewFileHunksRequest {
            ignore_whitespace: true,
            ..hunks_request(repo, &staged, &path)
        },
    );
    assert!(ignoring.patch_id.is_none());
    assert_eq!(added_lines(&ignoring), added_lines(&staged_hunks));
}

/// Run git with extra environment (temporary index, fixed identity).
fn git_env(repo: &Path, envs: &[(&str, &str)], args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@bcode.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@bcode.invalid")
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write stdin");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Create a commit on `main` that applies only layer 2's change to the stack's shared file, as a
/// rebase of layer 2 that drops layer 1 would.
fn rebased_layer_two(fixture: &GitFixture) -> String {
    let repo = fixture.root();
    let show = |spec: String| fixture.git(&["show", &spec]);
    let layer1 = show(format!("{}:{STACK_SHARED_FILE}", STACK_BRANCHES[0]));
    let layer2 = show(format!("{}:{STACK_SHARED_FILE}", STACK_BRANCHES[1]));
    let main = show(format!("{HEAD_BRANCH}:{STACK_SHARED_FILE}"));
    let rebased: String = main
        .lines()
        .zip(layer1.lines().zip(layer2.lines()))
        .map(|(main, (one, two))| if one == two { main } else { two })
        .flat_map(|line| [line, "\n"])
        .collect();
    let blob = git_env(
        repo,
        &[],
        &["hash-object", "-w", "--stdin"],
        Some(rebased.as_bytes()),
    );
    let index = repo.join(".git/rebase-test-index");
    let index = index.to_str().expect("utf-8 path");
    let envs = [("GIT_INDEX_FILE", index)];
    git_env(repo, &envs, &["read-tree", HEAD_BRANCH], None);
    let cacheinfo = format!("100644,{blob},{STACK_SHARED_FILE}");
    git_env(
        repo,
        &envs,
        &["update-index", "--cacheinfo", &cacheinfo],
        None,
    );
    let tree = git_env(repo, &envs, &["write-tree"], None);
    git_env(
        repo,
        &[],
        &[
            "commit-tree",
            &tree,
            "-p",
            HEAD_BRANCH,
            "-m",
            "rebased layer 2",
        ],
        None,
    )
}

#[test]
fn patch_identity_survives_a_rebase_like_rewrite_and_keeps_the_file_viewed() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let rebased = rebased_layer_two(&fixture);

    let original = resolve_one(
        &mut service,
        repo,
        compare(STACK_BRANCHES[0], STACK_BRANCHES[1]),
    );
    let rewritten = resolve_one(&mut service, repo, compare(HEAD_BRANCH, &rebased));
    let original_entry = entry(
        &full_index(&mut service, repo, &original),
        STACK_SHARED_FILE,
    )
    .clone();
    let rewritten_entry = entry(
        &full_index(&mut service, repo, &rewritten),
        STACK_SHARED_FILE,
    )
    .clone();
    assert_ne!(original_entry.old_blob, rewritten_entry.old_blob);
    assert_ne!(original_entry.new_blob, rewritten_entry.new_blob);

    let original_hunks = hunks(
        &mut service,
        &hunks_request(repo, &original, STACK_SHARED_FILE),
    );
    let rewritten_hunks = hunks(
        &mut service,
        &hunks_request(repo, &rewritten, STACK_SHARED_FILE),
    );
    assert!(original_hunks.patch_id.is_some());
    assert_eq!(original_hunks.patch_id, rewritten_hunks.patch_id);
    // Layer 3's change to the same file is a different patch.
    let layer3 = resolve_one(
        &mut service,
        repo,
        compare(STACK_BRANCHES[1], STACK_BRANCHES[2]),
    );
    let layer3_hunks = hunks(
        &mut service,
        &hunks_request(repo, &layer3, STACK_SHARED_FILE),
    );
    assert_ne!(layer3_hunks.patch_id, original_hunks.patch_id);

    // Marked on the original: the rewrite is unknown until hunks load, then viewed.
    let mut workspace = bcode_code_review_models::ReviewWorkspace::from_target(
        repo.to_path_buf(),
        bcode_code_review_models::ReviewTarget::Repository,
    );
    let original_identity = EntryIdentity::from_entry(&original_entry);
    mark_viewed(
        &mut workspace,
        &original_identity.with_patch(original_hunks.patch_id.as_deref()),
    );
    let rewritten_identity = EntryIdentity::from_entry(&rewritten_entry);
    assert_eq!(
        viewed_state(&workspace, &rewritten_identity.with_patch(None)),
        ViewedState::Unviewed
    );
    assert_eq!(
        viewed_state(
            &workspace,
            &rewritten_identity.with_patch(rewritten_hunks.patch_id.as_deref())
        ),
        ViewedState::ViewedNeedsUpdate
    );
    let layer3_entry = entry(&full_index(&mut service, repo, &layer3), STACK_SHARED_FILE).clone();
    assert_eq!(
        viewed_state(
            &workspace,
            &EntryIdentity::from_entry(&layer3_entry).with_patch(layer3_hunks.patch_id.as_deref())
        ),
        ViewedState::Unviewed
    );
}

// Blob lines and cat-file -----------------------------------------------------------------------

#[test]
fn blob_lines_read_each_revision_side_with_ranges() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let base = RevisionRef::commit(rev_parse(&fixture, BASE_BRANCH));
    let head = RevisionRef::commit(rev_parse(&fixture, HEAD_BRANCH));
    let path = GitFixture::modified_path(0);

    let head_text = fixture.git(&["show", &format!("{HEAD_BRANCH}:{LARGE_FILE}")]);
    let head_lines: Vec<&str> = head_text.lines().collect();
    let large_oid = rev_parse(&fixture, &format!("{HEAD_BRANCH}:{LARGE_FILE}"));
    let range = blob_lines(&mut service, repo, object(&large_oid), 100, 10).expect("range");
    assert_eq!(range.start_line, 100);
    assert_eq!(range.total_lines as usize, head_lines.len());
    assert_eq!(range.lines, head_lines[99..109]);

    let past_end = blob_lines(
        &mut service,
        repo,
        at(base.clone(), LARGE_FILE),
        1_000_000,
        10,
    )
    .expect("end");
    assert!(past_end.lines.is_empty() && past_end.total_lines > 0);

    let clamped = blob_lines(
        &mut service,
        repo,
        at(base.clone(), LARGE_FILE),
        1,
        u32::MAX,
    )
    .expect("clamp");
    assert_eq!(clamped.lines.len(), clamped.total_lines as usize);

    let old = blob_lines(&mut service, repo, at(base.clone(), &path), 1, 3).expect("base side");
    assert!(old.lines[0].contains("original line 0"));

    let staged = blob_lines(&mut service, repo, at(RevisionRef::Index, &path), 1, 1_000)
        .expect("index side");
    let staged_text = fixture.git(&["show", &format!(":{path}")]);
    assert_eq!(staged.lines, staged_text.lines().collect::<Vec<_>>());

    let disk = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::WorkingTree, &path),
        1,
        1_000,
    )
    .expect("working tree side");
    let disk_text = std::fs::read_to_string(repo.join(&path)).expect("read");
    assert_eq!(disk.lines, disk_text.lines().collect::<Vec<_>>());
    assert_ne!(
        staged.lines, disk.lines,
        "mixed file differs between index and disk"
    );

    let deleted_path = GitFixture::deleted_path(0);
    let deleted =
        blob_lines(&mut service, repo, at(base, &deleted_path), 1, 1).expect("deleted old side");
    assert_eq!(deleted.lines.len(), 1);
    let gone = blob_lines(&mut service, repo, at(head, &deleted_path), 1, 1);
    assert!(matches!(gone, Err(DataError::NotFound(_))), "{gone:?}");
}

#[test]
fn blob_lines_handle_crlf_binary_symlink_and_unicode_content() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let crlf = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::WorkingTree, CRLF_FILE),
        1,
        10,
    )
    .expect("crlf");
    assert_eq!(
        crlf.lines,
        vec!["first line", "middle edited line", "last line"]
    );

    let binary = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::Index, BINARY_FILE),
        1,
        10,
    )
    .expect("binary");
    assert!(binary.binary && binary.lines.is_empty());

    let link = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::WorkingTree, SYMLINK_FILE),
        1,
        1,
    )
    .expect("symlink");
    assert_eq!(link.lines, vec![SYMLINK_TARGET]);
    let committed_link = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::Index, SYMLINK_FILE),
        1,
        1,
    )
    .expect("committed symlink");
    assert_eq!(committed_link.lines, link.lines);

    let unicode = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::Index, UNICODE_FILE),
        3,
        1,
    )
    .expect("unicode");
    assert!(unicode.lines[0].contains("👩\u{200d}💻"));

    let empty = blob_lines(
        &mut service,
        repo,
        at(RevisionRef::EmptyTree, LARGE_FILE),
        1,
        1,
    );
    assert!(matches!(empty, Err(DataError::NotFound(_))));
}

#[test]
fn cat_file_process_is_reused_restarted_evicted_and_bounded() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let other = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let head = RevisionRef::commit(rev_parse(&fixture, HEAD_BRANCH));

    for line in 1..=3 {
        blob_lines(&mut service, repo, at(head.clone(), LARGE_FILE), line, 1).expect("read");
    }
    let registry = service.blobs_mut().registry();
    assert_eq!(registry.spawned(), 1, "one process serves every read");
    let pid = registry.pid(repo).expect("running process");

    service.blobs_mut().registry_mut().kill_for_test(repo);
    let after_crash = blob_lines(&mut service, repo, at(head.clone(), UNICODE_FILE), 1, 1)
        .expect("read after crash");
    assert_eq!(after_crash.lines.len(), 1);
    let registry = service.blobs_mut().registry();
    assert_eq!(registry.spawned(), 2, "crashed process restarted once");
    assert_ne!(registry.pid(repo), Some(pid));

    // Bounded: a one-process registry evicts the least recently used repository.
    *service.blobs_mut().registry_mut() = CatFileRegistry::new(1, Duration::from_secs(300));
    blob_lines(&mut service, repo, at(head.clone(), LARGE_FILE), 1, 1).expect("first repo");
    let other_head = RevisionRef::commit(rev_parse(&other, HEAD_BRANCH));
    blob_lines(&mut service, other.root(), at(other_head, LARGE_FILE), 1, 1).expect("second");
    let registry = service.blobs_mut().registry();
    assert_eq!(registry.len(), 1);
    assert!(registry.pid(repo).is_none() && registry.pid(other.root()).is_some());

    // Idle eviction.
    *service.blobs_mut().registry_mut() = CatFileRegistry::new(4, Duration::ZERO);
    blob_lines(&mut service, repo, at(head, LARGE_FILE), 1, 1).expect("read");
    std::thread::sleep(Duration::from_millis(5));
    service.blobs_mut().registry_mut().evict_idle();
    assert!(service.blobs_mut().registry().is_empty());
}

#[test]
fn object_reads_are_served_from_the_blob_cache() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let oid = rev_parse(&fixture, &format!("{HEAD_BRANCH}:{LARGE_FILE}"));
    blob_lines(&mut service, repo, object(&oid), 1, 1).expect("read");
    let pid = service.blobs_mut().registry().pid(repo);
    service.blobs_mut().registry_mut().kill_for_test(repo);
    blob_lines(&mut service, repo, object(&oid), 2, 1).expect("cached read");
    assert_eq!(
        service.blobs_mut().registry().spawned(),
        1,
        "no object read was needed"
    );
    service.blobs_mut().clear_cache();
    blob_lines(&mut service, repo, object(&oid), 2, 1).expect("uncached read");
    assert_ne!(service.blobs_mut().registry().pid(repo), pid);
}

#[test]
fn unconfined_paths_are_rejected() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.txt"), "secret\n").expect("write");
    std::os::unix::fs::symlink(outside.path(), repo.join("escape")).expect("symlink");

    for path in [
        "../secret.txt",
        "/etc/passwd",
        "escape/secret.txt",
        "a/../../secret.txt",
        ".git/config",
    ] {
        let result = blob_lines(&mut service, repo, at(RevisionRef::WorkingTree, path), 1, 1);
        assert!(
            matches!(result, Err(DataError::PathRejected(_))),
            "{path}: {result:?}"
        );
    }
    let index_side = blob_lines(&mut service, repo, at(RevisionRef::Index, "../x"), 1, 1);
    assert!(matches!(index_side, Err(DataError::PathRejected(_))));

    let source = resolve_one(&mut service, repo, branch_compare());
    let hunks = service.file_hunks_get(&hunks_request(repo, &source, "../x"));
    assert!(matches!(hunks, Err(DataError::PathRejected(_))));
    let index = service.index_get(&ReviewIndexRequest {
        repo_path: repo.to_path_buf(),
        source: ResolvedSource {
            paths: vec!["/etc".to_string()],
            ..source
        },
        cursor: None,
        limit: None,
    });
    assert!(matches!(index, Err(DataError::PathRejected(_))));
    let injected = blob_lines(&mut service, repo, object("--batch-all-objects"), 1, 1);
    assert!(matches!(injected, Err(DataError::InvalidRequest(_))));
}

#[test]
fn index_reads_stage_like_paths_and_type_changes_cover_both_sides() {
    let repo = tempfile::tempdir().expect("repo");
    let repo = repo.path();
    let git = |args: &[&str]| git_env(repo, &[], args, None);
    git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.join("1:x"), "stage-like name\n").expect("write");
    std::fs::write(repo.join("x"), "plain x\n").expect("write");
    std::fs::write(repo.join("kind.txt"), "regular file\n").expect("write");
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "-m", "base"]);
    std::fs::remove_file(repo.join("kind.txt")).expect("remove");
    std::os::unix::fs::symlink("x", repo.join("kind.txt")).expect("symlink");
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "-m", "type change"]);
    let mut service = ReviewData::default();

    let staged = blob_lines(&mut service, repo, at(RevisionRef::Index, "1:x"), 1, 1)
        .expect("stage-like path");
    assert_eq!(staged.lines, vec!["stage-like name"]);

    let source = resolve_one(&mut service, repo, ReviewSourceKind::LastCommit);
    let response = hunks(&mut service, &hunks_request(repo, &source, "kind.txt"));
    let lines: Vec<(ReviewLineKind, &str)> = response
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .map(|line| (line.kind, line.content.as_str()))
        .collect();
    assert_eq!(
        lines,
        vec![
            (ReviewLineKind::Removed, "regular file"),
            (ReviewLineKind::Added, "x"),
        ]
    );
    assert!(response.patch_id.is_some());
}

// Search and log --------------------------------------------------------------------------------

#[test]
fn refs_search_ranks_filters_and_pages() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    fixture.run(&["tag", "v1.0", BASE_BRANCH]);
    let request = |query: &str, cursor: Option<String>| ReviewRefsSearchRequest {
        repo_path: repo.to_path_buf(),
        query: query.to_string(),
        kinds: vec![ReviewRefKind::Branch],
        cursor,
        limit: Some(1),
    };
    let mut names = Vec::new();
    let mut cursor = None;
    loop {
        let page = service
            .refs_search(&request("stack", cursor))
            .expect("page");
        assert_eq!(page.total_matches, 3);
        names.extend(page.refs.into_iter().map(|entry| entry.name));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(names, STACK_BRANCHES.map(str::to_string));

    let first = service.refs_search(&request("stack", None)).expect("first");
    let changed_query = service.refs_search(&request("extra", first.next_cursor));
    assert!(matches!(changed_query, Err(DataError::CursorStale)));

    let main = service.refs_search(&request("main", None)).expect("main");
    assert_eq!(main.refs[0].name, HEAD_BRANCH);
    assert!(main.refs[0].is_head);

    let tags = service
        .refs_search(&ReviewRefsSearchRequest {
            kinds: vec![ReviewRefKind::Tag],
            ..request("", None)
        })
        .expect("tags");
    assert_eq!(tags.refs[0].name, "v1.0");
    assert_eq!(tags.refs[0].oid, rev_parse(&fixture, BASE_BRANCH));
}

#[test]
fn commit_pages_stay_anchored_when_the_branch_moves() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let page_request =
        |rev: Option<&str>, query: Option<&str>, cursor: Option<String>| ReviewCommitsPageRequest {
            repo_path: repo.to_path_buf(),
            rev: rev.map(str::to_string),
            query: query.map(str::to_string),
            cursor,
            limit: Some(2),
        };
    let first = service
        .commits_page(&page_request(Some(STACK_BRANCHES[2]), None, None))
        .expect("first page");
    let subjects: Vec<&str> = first
        .commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_eq!(subjects, vec!["stack layer 3", "stack layer 2"]);
    assert_eq!(
        first.commits[0].parents,
        vec![rev_parse(&fixture, STACK_BRANCHES[1])]
    );
    assert!(
        first.commits[0]
            .refs
            .iter()
            .any(|decoration| decoration.contains("stack-3"))
    );

    let moved = format!("refs/heads/{}", STACK_BRANCHES[2]);
    fixture.run(&["update-ref", &moved, BASE_BRANCH]);
    let mut subjects = Vec::new();
    let mut cursor = first.next_cursor;
    while let Some(next) = cursor {
        let page = service
            .commits_page(&page_request(None, None, Some(next)))
            .expect("page");
        subjects.extend(page.commits.into_iter().map(|commit| commit.subject));
        cursor = page.next_cursor;
    }
    assert_eq!(subjects, vec!["stack layer 1", "second", "initial"]);

    let filtered = service
        .commits_page(&page_request(
            Some(STACK_BRANCHES[1]),
            Some("LAYER 2"),
            None,
        ))
        .expect("filtered");
    assert_eq!(filtered.commits.len(), 1);
    assert!(
        service
            .commits_page(&page_request(Some("no-such-rev"), None, None))
            .is_err()
    );
}

#[test]
fn files_search_ranks_paths_per_revision_and_pages() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let request = |query: &str, revision: Option<RevisionRef>| ReviewFilesSearchRequest {
        repo_path: repo.to_path_buf(),
        query: query.to_string(),
        revision,
        cursor: None,
        limit: Some(2),
    };
    let huge = service
        .files_search(&request("huge", None))
        .expect("search");
    assert_eq!(huge.files[0].path, LARGE_FILE);

    let all = service.files_search(&request("rs", None)).expect("all");
    let mut path_count = all.files.len();
    let mut cursor = all.next_cursor;
    while let Some(next) = cursor {
        let page = service
            .files_search(&ReviewFilesSearchRequest {
                cursor: Some(next),
                ..request("rs", None)
            })
            .expect("page");
        path_count += page.files.len();
        cursor = page.next_cursor;
    }
    assert_eq!(path_count, all.total_matches as usize);

    let base = RevisionRef::commit(rev_parse(&fixture, BASE_BRANCH));
    let at_base = service
        .files_search(&request("added", Some(base)))
        .expect("base");
    assert_eq!(
        at_base.total_matches, 0,
        "added files do not exist at the base"
    );
    let untracked = service
        .files_search(&request("note", Some(RevisionRef::WorkingTree)))
        .expect("working tree");
    assert_eq!(untracked.files[0].path, UNTRACKED_FILE);
    let tracked = service
        .files_search(&request("untracked/note", None))
        .expect("index");
    assert_eq!(tracked.total_matches, 0);
}

// Change token and dispatch ---------------------------------------------------------------------

#[test]
fn change_token_tracks_head_index_and_working_tree() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let token = |service: &mut ReviewData, include_working_tree: bool| {
        service
            .sources_token(&ReviewSourcesTokenRequest {
                repo_path: repo.to_path_buf(),
                include_working_tree,
            })
            .expect("token")
            .token
    };
    let idle = token(&mut service, true);
    assert_eq!(token(&mut service, true), idle, "no change, same token");
    let without_worktree = token(&mut service, false);

    // Re-editing an already modified file changes no status line, but must change the token.
    let unstaged = repo.join(GitFixture::modified_path(1));
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&unstaged, "edited again, longer than before\n").expect("edit");
    let edited = token(&mut service, true);
    assert_ne!(edited, idle);
    assert_eq!(token(&mut service, false), without_worktree);

    fixture.run(&["add", &GitFixture::modified_path(1)]);
    assert_ne!(
        token(&mut service, false),
        without_worktree,
        "staging changes the index"
    );
}

// Persistence -----------------------------------------------------------------------------------

/// A workspace carrying every kind of review state plus a legacy viewed path and an unknown source.
fn stateful_workspace(repo: &Path) -> bcode_code_review_models::ReviewWorkspace {
    use bcode_code_review_models::{ReviewLayoutMode, ReviewPresentationState, ReviewedRevision};
    let mut workspace = bcode_code_review_models::ReviewWorkspace::from_target(
        repo.to_path_buf(),
        bcode_code_review_models::ReviewTarget::LastCommit,
    );
    workspace.id = "workspace-presentation".to_string();
    workspace.viewed_files.insert("legacy.rs".to_string());
    mark_viewed(
        &mut workspace,
        &ViewedIdentity {
            path: "src/a.rs",
            old_oid: Some("o"),
            new_oid: Some("n"),
            patch_id: Some("p"),
        },
    );
    workspace.reviewed_revisions.insert(
        "source-1".to_string(),
        ReviewedRevision {
            reviewed_base_oid: Some("b".to_string()),
            reviewed_head_oid: Some("h".to_string()),
            recorded_at_ms: Some(1),
        },
    );
    workspace.presentation = Some(ReviewPresentationState {
        layout_mode: Some(ReviewLayoutMode::Unified),
        ..ReviewPresentationState::default()
    });
    // An unknown source kind written by a newer version survives the round trip.
    workspace.sources.push(source(
        "future",
        serde_json::from_str(r#"{"kind":"from_the_future","x":1}"#).expect("kind"),
    ));

    workspace
}

#[test]
fn workspace_review_state_persists_and_old_rows_read_as_empty() {
    use crate::store::{CodeReviewConfig, CodeReviewStateLocation, ReviewStore};
    use bcode_code_review_models::{GetReviewWorkspaceRequest, UpdateReviewWorkspaceRequest};
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root().to_path_buf();
    let state = tempfile::tempdir().expect("state");
    let config = CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(state.path().to_path_buf()),
    };
    let workspace = stateful_workspace(&repo);
    let store = ReviewStore::new(config.clone());
    store
        .update_workspace(UpdateReviewWorkspaceRequest {
            repo_path: repo.clone(),
            workspace: workspace.clone(),
        })
        .expect("save");
    let get = || {
        store
            .get_workspace(GetReviewWorkspaceRequest {
                repo_path: repo.clone(),
                workspace_id: "workspace-presentation".to_string(),
            })
            .expect("get")
            .workspace
            .expect("workspace")
    };
    let loaded = get();
    assert_eq!(loaded.viewed_marks, workspace.viewed_marks);
    assert_eq!(loaded.reviewed_revisions, workspace.reviewed_revisions);
    assert_eq!(loaded.presentation, workspace.presentation);
    assert_eq!(loaded.sources, workspace.sources);
    assert_eq!(
        effective_marks(&loaded).len(),
        2,
        "legacy path mark is migrated"
    );

    // Rows from before the review state columns (NULL) read as empty; unreadable presentation is dropped.
    crate::store::with_database(&repo, &config, |database| {
        Box::pin(async move {
            use switchy::database::DatabaseValue;
            use switchy::database::query::{FilterableQuery as _, where_eq};
            database
                .update("review_workspaces")
                .value("viewed_marks_json", DatabaseValue::Null)
                .value("reviewed_revisions_json", DatabaseValue::Null)
                .value(
                    "presentation_v2_json",
                    "{\"schema_version\":99,\"layout_mode\":\"holo\"}",
                )
                .filter(Box::new(where_eq("workspace_id", "workspace-presentation")))
                .execute(database)
                .await?;
            Ok(())
        })
    })
    .expect("downgrade row");
    let legacy = get();
    assert!(legacy.viewed_marks.is_empty() && legacy.reviewed_revisions.is_empty());
    assert!(legacy.presentation.is_none());
    let marks = effective_marks(&legacy);
    assert!(
        marks
            .iter()
            .all(bcode_code_review_models::ViewedMark::is_legacy)
    );
    assert_eq!(marks.len(), 2, "viewed paths become unknown-identity marks");

    // A well-formed value from a newer presentation schema is dropped, not read as this one.
    crate::store::with_database(&repo, &config, |database| {
        Box::pin(async move {
            use switchy::database::query::{FilterableQuery as _, where_eq};
            database
                .update("review_workspaces")
                .value("presentation_v2_json", "{\"schema_version\":99}")
                .filter(Box::new(where_eq("workspace_id", "workspace-presentation")))
                .execute(database)
                .await?;
            Ok(())
        })
    })
    .expect("newer presentation row");
    assert!(get().presentation.is_none());
}
