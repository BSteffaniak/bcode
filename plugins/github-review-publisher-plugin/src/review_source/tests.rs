//! Review-source provider tests: stack walks over synthetic pull-request lists, listing order,
//! repository naming, and fetching from a local remote (no network).

use super::*;
use bcode_forge::models::{PullRequestState, Transport};
use std::convert::Infallible;
use std::path::PathBuf;

/// Branch lookups answered from a fixed list of open pull requests, recording each branch asked.
struct Listed<'a> {
    pulls: &'a [PullRequestSummary],
    asked: Vec<String>,
}

impl StackBranches for Listed<'_> {
    type Error = Infallible;

    fn neighbors(&mut self, branch: &str) -> Result<BranchNeighbors, Infallible> {
        self.asked.push(branch.to_owned());
        let matching = |side: fn(&PullRequestSummary) -> &str| {
            self.pulls
                .iter()
                .filter(|pull| side(pull) == branch)
                .cloned()
                .collect()
        };
        Ok(BranchNeighbors {
            with_head: matching(|pull| &pull.head_branch),
            with_base: matching(|pull| &pull.base_branch),
        })
    }
}

/// Walk the stack of `requested` over `pulls`, returning it and the branches looked up.
fn walk(
    pulls: &[PullRequestSummary],
    default_branch: Option<&str>,
    requested: u64,
    max_layers: Option<usize>,
) -> Option<(ReviewStack, Vec<String>)> {
    let start = pulls.iter().find(|pull| pull.number == requested)?;
    let mut listed = Listed {
        pulls,
        asked: Vec::new(),
    };
    let stack = match walk_stack(&mut listed, start, default_branch, max_layers) {
        Ok(stack) => stack,
        Err(never) => match never {},
    };
    Some((stack, listed.asked))
}

fn detect_stack(
    pulls: &[PullRequestSummary],
    default_branch: Option<&str>,
    requested: u64,
) -> Option<ReviewStack> {
    walk(pulls, default_branch, requested, None).map(|(stack, _)| stack)
}

/// A chain of `layers` pull requests `l0 ← l1 ← …` on `main`, numbered from 1.
fn chain(layers: u64) -> Vec<PullRequestSummary> {
    (0..layers)
        .map(|index| {
            let base = if index == 0 {
                "main".to_string()
            } else {
                format!("l{}", index - 1)
            };
            pr(index + 1, &base, &format!("l{index}"))
        })
        .collect()
}

fn pr(number: u64, base: &str, head: &str) -> PullRequestSummary {
    PullRequestSummary {
        number,
        title: format!("Change {number}"),
        author: Some("octocat".into()),
        base_branch: base.into(),
        head_branch: head.into(),
        head_oid: format!("{number:040}"),
        base_oid: None,
        review_requested_from_me: false,
        draft: false,
        updated: Some(format!("2026-11-{:02}T00:00:00Z", number % 28 + 1)),
        url: None,
    }
}

fn numbers(stack: &ReviewStack) -> Vec<u64> {
    stack.layers.iter().map(|layer| layer.pull.number).collect()
}

fn parents(stack: &ReviewStack) -> Vec<Option<u64>> {
    stack.layers.iter().map(|layer| layer.parent).collect()
}

fn sibling_pairs(stack: &ReviewStack) -> Vec<(u64, u64)> {
    stack
        .siblings
        .iter()
        .map(|sibling| (sibling.parent, sibling.pull.number))
        .collect()
}

#[test]
fn linear_chains_are_found_from_any_layer() {
    let pulls = [
        pr(3, "b", "c"),
        pr(1, "main", "a"),
        pr(9, "main", "unrelated"),
        pr(2, "a", "b"),
    ];
    for requested in [1, 2, 3] {
        let stack = detect_stack(&pulls, Some("main"), requested).unwrap();
        assert_eq!(numbers(&stack), vec![1, 2, 3], "from #{requested}");
        assert_eq!(parents(&stack), vec![None, Some(1), Some(2)]);
        assert_eq!(stack.trunk, "main");
        assert!(stack.rooted_at_default_branch);
        assert_eq!(stack.requested, requested);
        assert!(stack.siblings.is_empty() && stack.warnings.is_empty());
    }
    // A pull request straight against the trunk with nothing on top is a one-layer stack.
    let single = detect_stack(&pulls, Some("main"), 9).unwrap();
    assert_eq!(numbers(&single), vec![9]);
    assert!(detect_stack(&pulls, Some("main"), 42).is_none());
}

#[test]
fn branching_returns_the_requested_chain_and_reports_siblings() {
    // #1 has two children (#2 and #5); #2 continues with #3.
    let pulls = [
        pr(1, "main", "a"),
        pr(2, "a", "b"),
        pr(3, "b", "c"),
        pr(5, "a", "e"),
    ];
    let stack = detect_stack(&pulls, Some("main"), 3).unwrap();
    assert_eq!(numbers(&stack), vec![1, 2, 3]);
    assert_eq!(sibling_pairs(&stack), vec![(1, 5)]);
    assert!(stack.warnings.is_empty(), "{:?}", stack.warnings);

    let stack = detect_stack(&pulls, Some("main"), 5).unwrap();
    assert_eq!(numbers(&stack), vec![1, 5]);
    assert_eq!(sibling_pairs(&stack), vec![(1, 2)]);

    // From the branching layer itself the chain cannot continue up unambiguously.
    let stack = detect_stack(&pulls, Some("main"), 1).unwrap();
    assert_eq!(numbers(&stack), vec![1]);
    assert_eq!(sibling_pairs(&stack), vec![(1, 2), (1, 5)]);
}

#[test]
fn a_missing_middle_layer_ends_the_chain_with_a_warning() {
    // The pull request for branch `b` (between #1 and #3) is closed, so it is not listed.
    let pulls = [pr(1, "main", "a"), pr(3, "b", "c"), pr(4, "c", "d")];
    let stack = detect_stack(&pulls, Some("main"), 4).unwrap();
    assert_eq!(numbers(&stack), vec![3, 4]);
    assert_eq!(stack.trunk, "b");
    assert!(!stack.rooted_at_default_branch);
    assert_eq!(stack.warnings.len(), 1);
    assert!(
        stack.warnings[0].contains("missing a layer"),
        "{:?}",
        stack.warnings
    );
    // From below the gap, the upper part is not reachable.
    let lower = detect_stack(&pulls, Some("main"), 1).unwrap();
    assert_eq!(numbers(&lower), vec![1]);
    assert!(lower.rooted_at_default_branch);
    // Without a known default branch no warning can be given.
    let unknown = detect_stack(&pulls, None, 4).unwrap();
    assert_eq!(numbers(&unknown), vec![3, 4]);
    assert!(unknown.warnings.is_empty() && !unknown.rooted_at_default_branch);
}

#[test]
fn cycles_are_guarded() {
    let pulls = [pr(1, "b", "a"), pr(2, "a", "b")];
    for requested in [1, 2] {
        let stack = detect_stack(&pulls, Some("main"), requested).unwrap();
        let layers = numbers(&stack);
        let unique: BTreeSet<u64> = layers.iter().copied().collect();
        assert_eq!(unique.len(), layers.len(), "{layers:?}");
        assert!(layers.len() <= 2);
        assert!(
            stack
                .warnings
                .iter()
                .any(|warning| warning.contains("cycle")),
            "{:?}",
            stack.warnings
        );
    }
    // A pull request whose base is its own head.
    let stack = detect_stack(&[pr(7, "x", "x")], Some("main"), 7).unwrap();
    assert_eq!(numbers(&stack), vec![7]);
    assert!(
        stack
            .warnings
            .iter()
            .any(|warning| warning.contains("cycle"))
    );
}

#[test]
fn duplicate_head_branches_below_use_the_latest_pull_request() {
    let mut older = pr(1, "main", "a");
    older.updated = Some("2026-01-01T00:00:00Z".into());
    let mut newer = pr(2, "main", "a");
    newer.updated = Some("2026-06-01T00:00:00Z".into());
    let pulls = [older, newer, pr(3, "a", "c")];
    let stack = detect_stack(&pulls, Some("main"), 3).unwrap();
    assert_eq!(numbers(&stack), vec![2, 3]);
    assert!(
        stack.warnings[0].contains("using #2"),
        "{:?}",
        stack.warnings
    );
}

#[test]
fn a_layer_limit_cuts_the_chain_with_a_warning() {
    let pulls = chain(100);
    let (stack, _) = walk(&pulls, Some("main"), 50, Some(64)).unwrap();
    assert_eq!(stack.layers.len(), 64);
    assert!(stack.layers.iter().any(|layer| layer.pull.number == 50));
    // Every layer below #50 fits, so the layers above #64 are the ones left out.
    assert_eq!(stack.layers.first().map(|layer| layer.pull.number), Some(1));
    assert_eq!(stack.layers.last().map(|layer| layer.pull.number), Some(64));
    assert_eq!(stack.warnings.len(), 1, "{:?}", stack.warnings);
    assert!(
        stack.warnings[0].contains("more than 64 layers"),
        "{:?}",
        stack.warnings
    );
    // The layer cut off above is the chain's continuation, not a sibling.
    assert!(stack.siblings.is_empty(), "{:?}", sibling_pairs(&stack));
    // Without a limit the whole chain is walked.
    let whole = detect_stack(&pulls, Some("main"), 50).unwrap();
    assert_eq!(numbers(&whole), (1..=100).collect::<Vec<u64>>());
    assert!(whole.warnings.is_empty(), "{:?}", whole.warnings);
}

#[test]
fn deep_stacks_are_walked_whole_with_one_lookup_per_layer() {
    // An 87-layer stack from its second layer: the depth that motivated walking branches
    // instead of listing the repository's most recent pull requests.
    let mut pulls = chain(87);
    pulls.push(pr(500, "main", "unrelated"));
    let (stack, asked) = walk(&pulls, Some("main"), 2, None).unwrap();
    assert_eq!(numbers(&stack), (1..=87).collect::<Vec<u64>>());
    assert!(stack.rooted_at_default_branch);
    assert!(stack.warnings.is_empty(), "{:?}", stack.warnings);
    // Each layer's head branch once; the default branch, which may have thousands of pull
    // requests, never.
    let unique: BTreeSet<&String> = asked.iter().collect();
    assert_eq!(unique.len(), asked.len(), "{asked:?}");
    assert_eq!(asked.len(), 87);
    assert!(!asked.iter().any(|branch| branch == "main"));
}

#[test]
fn a_failed_lookup_fails_the_walk() {
    struct Offline;
    impl StackBranches for Offline {
        type Error = ReviewSourceError;
        fn neighbors(&mut self, _branch: &str) -> Result<BranchNeighbors, ReviewSourceError> {
            Err(ReviewSourceError::Failed("GitHub is unreachable".into()))
        }
    }
    let start = pr(2, "a", "b");
    let error = walk_stack(&mut Offline, &start, Some("main"), None).unwrap_err();
    assert_eq!(error.code(), REVIEW_SOURCE_ERROR_FAILED);
}

fn forge_pull(number: u64, state: PullRequestState, head_repository: &str) -> PullRequest {
    PullRequest {
        number,
        title: format!("#{number}"),
        url: format!("https://github.com/octo/hello/pull/{number}"),
        state,
        head: format!("b{number}"),
        head_repository: Some(head_repository.into()),
        head_oid: format!("{number:040}"),
        base: "main".into(),
        review: None,
        checks: None,
        updated_at: format!("2026-11-{number:02}T00:00:00Z"),
        author: Some("octocat".into()),
        base_oid: Some("b".repeat(40)),
        review_requested_from_viewer: false,
    }
}

fn octo_hello() -> Repository {
    Repository(parse_repository("octo/hello").unwrap())
}

#[test]
fn pull_lists_show_open_pulls_with_requested_reviews_first() {
    let mut requested = forge_pull(2, PullRequestState::Open, "octo/hello");
    requested.review_requested_from_viewer = true;
    let list = PullRequestList {
        pull_requests: vec![
            forge_pull(1, PullRequestState::Open, "octo/hello"),
            requested,
            forge_pull(3, PullRequestState::Draft, "someone/hello"),
            forge_pull(4, PullRequestState::Merged, "octo/hello"),
            forge_pull(5, PullRequestState::Closed, "octo/hello"),
        ],
        via: Transport::Cli,
        default_branch: Some("main".into()),
    };
    let response = pulls_list_response(&octo_hello(), list.clone());
    assert_eq!(response.repository, "octo/hello");
    assert_eq!(response.default_branch.as_deref(), Some("main"));
    let order: Vec<u64> = response.pulls.iter().map(|pull| pull.number).collect();
    assert_eq!(order, vec![2, 3, 1]);
    let draft = &response.pulls[1];
    assert!(draft.draft);
    assert_eq!(draft.author.as_deref(), Some("octocat"));
    assert_eq!(draft.base_oid.as_deref(), Some("b".repeat(40).as_str()));
    assert!(response.pulls[0].review_requested_from_me);

    // Forks cannot be stack layers: their head branches live in another repository.
    let candidates: Vec<u64> = stack_candidates(&octo_hello(), &list.pull_requests)
        .iter()
        .map(|pull| pull.number)
        .collect();
    assert_eq!(candidates, vec![1, 2]);
}

#[test]
fn lookups_name_numbers_urls_or_branches() {
    for query in [
        "49230",
        "#49230",
        "https://github.com/o/r/pull/49230",
        "o/r/pull/49230/files",
    ] {
        assert_eq!(parse_pull_query(query), PullQuery::Number(49230), "{query}");
    }
    for branch in [
        "sa-17343-lob-37-mn-branches",
        "feature/1234",
        "#feature",
        "123abc",
    ] {
        assert_eq!(
            parse_pull_query(branch),
            PullQuery::Branch(branch),
            "{branch}"
        );
    }
}

#[test]
fn repository_identifiers_parse_with_optional_hosts() {
    let parsed = |identifier: &str| {
        parse_repository(identifier).map(|repository| {
            (
                repository.host.clone(),
                repository.full_name(),
                Repository(repository).display(),
            )
        })
    };
    assert_eq!(
        parsed("octo/hello"),
        Some((
            "github.com".into(),
            "octo/hello".into(),
            "octo/hello".into()
        ))
    );
    assert_eq!(
        parsed("GHE.example.com/team/tool.git"),
        Some((
            "ghe.example.com".into(),
            "team/tool".into(),
            "ghe.example.com/team/tool".into()
        ))
    );
    for invalid in ["", "octo", "a/b/c/d", "octo/he llo", "octo/-x;rm"] {
        assert_eq!(parsed(invalid).map(|parsed| parsed.1), None, "{invalid:?}");
    }
}

fn run(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// An "upstream" repository with `main`, a pull request head at `refs/pull/7/head`, and a clone
/// of it whose `origin` is the upstream path.
fn upstream_and_clone() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let upstream = dir.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    run(&upstream, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(upstream.join("a.txt"), "one\n").unwrap();
    run(&upstream, &["add", "a.txt"]);
    run(&upstream, &["commit", "--quiet", "-m", "base"]);
    run(&upstream, &["clone", "--quiet", ".", "../clone"]);
    // Work that only exists on the pull request ref, plus a newer base.
    run(&upstream, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(upstream.join("a.txt"), "one\ntwo\n").unwrap();
    run(&upstream, &["commit", "--quiet", "-am", "feature"]);
    run(&upstream, &["update-ref", "refs/pull/7/head", "HEAD"]);
    run(&upstream, &["checkout", "--quiet", "main"]);
    run(&upstream, &["branch", "--quiet", "-D", "feature"]);
    std::fs::write(upstream.join("b.txt"), "base\n").unwrap();
    run(&upstream, &["add", "b.txt"]);
    run(&upstream, &["commit", "--quiet", "-m", "newer base"]);
    let clone = dir.path().join("clone");
    (dir, upstream, clone)
}

#[test]
fn fetching_a_pull_request_writes_review_owned_refs_only() {
    let (_dir, upstream, clone) = upstream_and_clone();
    let refs_before = run(
        &clone,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    let result = fetch_pull(&clone, "origin", 7, "main").expect("fetch");
    assert_eq!(result.number, 7);
    assert_eq!(result.head_ref, "refs/bcode-review/pull/7/head");
    assert_eq!(result.base_ref, "refs/bcode-review/pull/7/base");
    assert_eq!(
        result.head_oid,
        run(&upstream, &["rev-parse", "refs/pull/7/head"])
    );
    assert_eq!(result.base_oid, run(&upstream, &["rev-parse", "main"]));
    assert_eq!(result.base_branch, "main");
    // The commits exist locally; the user's branches and remote-tracking refs are untouched.
    run(
        &clone,
        &["cat-file", "-e", &format!("{}^{{commit}}", result.head_oid)],
    );
    let refs_after = run(
        &clone,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    let new_refs: Vec<&str> = refs_after
        .lines()
        .filter(|line| !refs_before.lines().any(|before| before == *line))
        .collect();
    assert_eq!(new_refs.len(), 2, "{new_refs:?}");
    assert!(
        new_refs
            .iter()
            .all(|line| line.starts_with(FETCH_REF_NAMESPACE)),
        "{new_refs:?}"
    );
    // Fetching again is idempotent.
    assert_eq!(fetch_pull(&clone, "origin", 7, "main").unwrap(), result);
}

#[test]
fn batched_fetches_use_local_commits_and_fetch_the_rest_at_once() {
    let (_dir, upstream, clone) = upstream_and_clone();
    let local_main = run(&clone, &["rev-parse", "main"]);
    let target =
        |number: u64, head_oid: Option<String>, base_oid: Option<String>| ReviewPullFetchTarget {
            number,
            base_branch: "main".into(),
            head_oid,
            base_oid,
        };
    // Commits that are already local need no remote at all.
    let local = fetch_pulls(
        &clone,
        "no-such-remote",
        &[target(
            5,
            Some(local_main.clone()),
            Some(local_main.clone()),
        )],
    )
    .expect("local fetch");
    assert_eq!(local[0].head_oid, local_main);
    assert_eq!(local[0].base_oid, local_main);
    assert_eq!(
        run(&clone, &["rev-parse", "refs/bcode-review/pull/5/head"]),
        local_main
    );
    // A commit that is not local (and an unreported one) arrive in one fetch, beside a local one.
    let pull_head = run(&upstream, &["rev-parse", "refs/pull/7/head"]);
    let fetched = fetch_pulls(
        &clone,
        "origin",
        &[
            target(7, Some(pull_head.clone()), None),
            target(5, Some(local_main.clone()), Some(local_main.clone())),
        ],
    )
    .expect("fetch");
    assert_eq!(
        fetched
            .iter()
            .map(|result| result.number)
            .collect::<Vec<_>>(),
        [7, 5]
    );
    assert_eq!(fetched[0].head_oid, pull_head);
    assert_eq!(fetched[0].base_oid, run(&upstream, &["rev-parse", "main"]));
    assert_eq!(fetched[1].head_oid, local_main);
    // Unsafe names are refused before anything runs.
    assert!(matches!(
        fetch_pulls(&clone, "--upload-pack=x", &[target(5, None, None)]),
        Err(ReviewSourceError::InvalidRequest(_))
    ));
}

#[test]
fn fetch_rejects_unsafe_names_and_reports_missing_pulls() {
    let (_dir, _upstream, clone) = upstream_and_clone();
    for base in ["", "-x", "a..b", "bad\nname"] {
        assert!(
            matches!(
                fetch_pull(&clone, "origin", 7, base),
                Err(ReviewSourceError::InvalidRequest(_))
            ),
            "{base:?}"
        );
    }
    assert!(matches!(
        fetch_pull(&clone, "--upload-pack=x", 7, "main"),
        Err(ReviewSourceError::InvalidRequest(_))
    ));
    let missing = fetch_pull(&clone, "origin", 99, "main").unwrap_err();
    assert_eq!(missing.code(), REVIEW_SOURCE_ERROR_FETCH_FAILED);
}

#[test]
fn repositories_are_detected_from_remotes() {
    let (_dir, _upstream, clone) = upstream_and_clone();
    // The clone's origin is a local path, which names no GitHub repository.
    assert!(matches!(
        Repository::resolve(&clone, None),
        Err(ReviewSourceError::UnsupportedRepository(_))
    ));
    run(
        &clone,
        &["remote", "add", "github", "git@github.com:Octo/Hello.git"],
    );
    let repository = Repository::resolve(&clone, None).unwrap();
    assert_eq!(repository.display(), "Octo/Hello");
    // The matching remote is used for fetching (case-insensitively); other repositories fall
    // back to their HTTPS URL.
    assert_eq!(octo_hello().fetch_remote(&clone), "github");
    let other = Repository(parse_repository("someone/else").unwrap());
    assert_eq!(
        other.fetch_remote(&clone),
        "https://github.com/someone/else.git"
    );
}

#[test]
fn the_manifest_and_unknown_operations_answer_through_the_service() {
    let request = |operation: &str, payload: &[u8]| ServiceRequest {
        interface_id: bcode_code_review_models::REVIEW_SOURCE_INTERFACE_ID.into(),
        operation: operation.into(),
        payload: payload.to_vec(),
    };
    let response = invoke(&request(OP_REVIEW_SOURCE_MANIFEST, b"{}"));
    assert!(response.error.is_none());
    let manifest: ReviewSourceProviderManifest = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(manifest.id, PROVIDER_ID);
    assert!(manifest.capabilities.stack_resolve && manifest.capabilities.pull_fetch);
    let unknown = invoke(&request("review_source.nope", b"{}"));
    assert_eq!(unknown.error.unwrap().code, "unsupported_operation");
    let invalid = invoke(&request(OP_REVIEW_SOURCE_PULLS_LIST, b"not json"));
    assert_eq!(invalid.error.unwrap().code, "invalid_request");
}
