//! Latency budgets of opening a step-through review of a 100-layer stack (see "Performance
//! budgets" in `docs/review-loading-architecture.md`).
//!
//! A synthetic repository holds the stack's commits locally; a provider answers about it with a
//! fixed delay per round trip (three for a stack walk), as a forge would, and fetches locally (as
//! GitHub's provider does when the commits are already there).
//!
//! What makes a path fast does not depend on the machine: how many provider round trips and git
//! processes it needs before its first frame and in all. Those are asserted in every test run.
//! How long it takes does depend on the machine (starting a git process costs from 2 ms to 40 ms),
//! so the timings are asserted by an ignored test, run alone in release.

use super::*;
use crate::cache::ReviewCache;
use crate::provider::{
    ProviderCallError, ProviderResult, ReviewSourceProvider, resolve_stack as resolve_through,
};
use bcode_code_review_models::{
    PullRequestSummary, ReviewPullFetchResult, ReviewSourceBatchCapabilities,
    ReviewSourceCapabilities, ReviewSourceKind, ReviewSourceProviderManifest,
    ReviewSourcePullFetchRequest, ReviewSourcePullsFetchRequest, ReviewSourcePullsFetchResponse,
    ReviewSourcePullsGetRequest, ReviewSourcePullsGetResponse, ReviewSourcePullsListRequest,
    ReviewSourcePullsListResponse, ReviewSourceStackResolveRequest,
    ReviewSourceStackResolveResponse, ReviewStack, ReviewStackLayer,
};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LAYERS: u64 = 100;
const PROVIDER: &str = "chain";
const REPOSITORY: &str = "octo/chain";
/// One provider round trip.
const ROUND_TRIP: Duration = Duration::from_millis(250);
/// The first frame, once the stack is known.
const FIRST_FRAME: Duration = Duration::from_millis(50);
/// Every layer listed.
const EVERY_LAYER: Duration = Duration::from_secs(1);
/// Slack for a busy machine: budgets are checked against `budget * SLACK`.
const SLACK: u32 = 4;

/// A repository whose `main` has one commit and whose branches `layer-1` … `layer-100` each add
/// a commit (touching a few files) on the one below: built with one `git fast-import`.
fn chain_repository(root: &Path) -> Vec<String> {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    let mut stream = String::new();
    let mut mark = 1;
    let commit = |stream: &mut String, branch: &str, mark: usize, parent: Option<usize>, n: u64| {
        use std::fmt::Write as _;
        let _ = writeln!(stream, "commit refs/heads/{branch}\nmark :{mark}");
        let _ = writeln!(
            stream,
            "committer T <t@example.com> {} +0000",
            1_700_000_000 + n
        );
        let message = format!("layer {n}");
        let _ = writeln!(stream, "data {}\n{message}", message.len());
        if let Some(parent) = parent {
            let _ = writeln!(stream, "from :{parent}");
        }
        for file in 0..3 {
            let body = format!("layer {n} file {file}\n");
            let _ = writeln!(
                stream,
                "M 100644 inline layers/{n:03}/{file}.txt\ndata {}\n{body}",
                body.len()
            );
        }
        let _ = writeln!(stream);
    };
    commit(&mut stream, "main", mark, None, 0);
    for layer in 1..=LAYERS {
        commit(
            &mut stream,
            &format!("layer-{layer}"),
            mark + 1,
            Some(mark),
            layer,
        );
        mark += 1;
    }
    let mut import = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .spawn()
        .expect("fast-import");
    import
        .stdin
        .take()
        .expect("stdin")
        .write_all(stream.as_bytes())
        .expect("write");
    assert!(import.wait().expect("fast-import").success());
    let output = Command::new("git")
        .args(["rev-parse", "main"])
        .args((1..=LAYERS).map(|layer| format!("layer-{layer}")))
        .current_dir(root)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A forge for the chain: stack resolution and lookups by number take [`ROUND_TRIP`]; fetching
/// finds the commits locally.
struct ChainProvider {
    /// `main`'s commit, then each layer's.
    oids: Vec<String>,
    resolves: AtomicUsize,
    gets: AtomicUsize,
}

impl ChainProvider {
    fn pull(&self, number: u64) -> PullRequestSummary {
        let index = usize::try_from(number).expect("index");
        PullRequestSummary {
            number,
            title: format!("Layer {number}"),
            author: None,
            base_branch: if number == 1 {
                "main".to_owned()
            } else {
                format!("layer-{}", number - 1)
            },
            head_branch: format!("layer-{number}"),
            head_oid: self.oids[index].clone(),
            base_oid: Some(self.oids[index - 1].clone()),
            review_requested_from_me: false,
            draft: false,
            updated: None,
            url: None,
        }
    }

    fn fetched(&self, number: u64, base_branch: &str) -> ReviewPullFetchResult {
        let pull = self.pull(number);
        let base_oid = if base_branch == "main" {
            self.oids[0].clone()
        } else {
            pull.base_oid.clone().expect("base")
        };
        ReviewPullFetchResult {
            number,
            head_ref: format!("refs/heads/{}", pull.head_branch),
            head_oid: pull.head_oid,
            base_branch: base_branch.to_owned(),
            base_ref: format!("refs/heads/{base_branch}"),
            base_oid,
        }
    }
}

impl ReviewSourceProvider for ChainProvider {
    fn manifest(&self) -> ReviewSourceProviderManifest {
        ReviewSourceProviderManifest {
            id: PROVIDER.to_owned(),
            label: "Chain".to_owned(),
            capabilities: ReviewSourceCapabilities {
                pulls_list: false,
                stack_resolve: true,
                pull_fetch: true,
                batched: ReviewSourceBatchCapabilities {
                    pulls_fetch: true,
                    pulls_get: true,
                    pulls_history: false,
                },
            },
        }
    }

    fn list_pulls(
        &self,
        _: &ReviewSourcePullsListRequest,
    ) -> ProviderResult<ReviewSourcePullsListResponse> {
        Err(ProviderCallError::new(
            "unsupported_operation",
            "no listing",
        ))
    }

    fn get_pulls(
        &self,
        request: &ReviewSourcePullsGetRequest,
    ) -> ProviderResult<ReviewSourcePullsGetResponse> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(ROUND_TRIP);
        Ok(ReviewSourcePullsGetResponse {
            repository: REPOSITORY.to_owned(),
            pulls: request
                .numbers
                .iter()
                .map(|number| self.pull(*number))
                .collect(),
            not_open: Vec::new(),
            // Nothing is stacked on the top layer.
            children: request
                .children_of
                .iter()
                .map(|branch| (branch.clone(), Vec::new()))
                .collect(),
        })
    }

    fn resolve_stack(
        &self,
        request: &ReviewSourceStackResolveRequest,
    ) -> ProviderResult<ReviewSourceStackResolveResponse> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        // A stack walk takes several round trips.
        std::thread::sleep(ROUND_TRIP * 3);
        Ok(ReviewSourceStackResolveResponse {
            repository: REPOSITORY.to_owned(),
            stack: ReviewStack {
                trunk: "main".to_owned(),
                rooted_at_default_branch: true,
                layers: (1..=LAYERS)
                    .map(|number| ReviewStackLayer {
                        pull: self.pull(number),
                        parent: number.checked_sub(1).filter(|below| *below > 0),
                    })
                    .collect(),
                requested: request.number,
                siblings: Vec::new(),
                warnings: Vec::new(),
            },
        })
    }

    fn fetch_pull(
        &self,
        request: &ReviewSourcePullFetchRequest,
    ) -> ProviderResult<ReviewPullFetchResult> {
        let base = request
            .base_branch
            .clone()
            .unwrap_or_else(|| self.pull(request.number).base_branch);
        Ok(self.fetched(request.number, &base))
    }

    fn fetch_pulls(
        &self,
        request: &ReviewSourcePullsFetchRequest,
    ) -> ProviderResult<ReviewSourcePullsFetchResponse> {
        Ok(ReviewSourcePullsFetchResponse {
            fetched: request
                .pulls
                .iter()
                .map(|pull| self.fetched(pull.number, &pull.base_branch))
                .collect(),
        })
    }
}

fn step_through(root: &Path, first: u64) -> OpenRequest {
    OpenRequest {
        repo_path: root.to_path_buf(),
        sources: (1..=LAYERS)
            .map(|number| ReviewSource {
                id: format!("layer-{number}"),
                kind: ReviewSourceKind::StackLayer {
                    provider: PROVIDER.into(),
                    repository: REPOSITORY.into(),
                    number,
                },
                label: format!("#{number}"),
                included: true,
            })
            .collect(),
        first: Some(format!("layer-{first}")),
        max_layers: None,
        cancel: crate::provider::Cancel::default(),
    }
}

/// One open of a path: when its first frame and every layer were ready, and what it asked of git
/// and the provider before its first frame and in all.
struct Measured {
    first_frame: Duration,
    every_layer: Duration,
    git_first: Vec<String>,
    git_all: Vec<String>,
    /// Stack resolutions and `pulls.get` requests.
    calls_first: (usize, usize),
    calls_all: (usize, usize),
}

fn measure(
    data: &ReviewData,
    registry: &ProviderRegistry,
    request: &OpenRequest,
    provider: &ChainProvider,
) -> Measured {
    let calls = || {
        (
            provider.resolves.load(Ordering::SeqCst),
            provider.gets.load(Ordering::SeqCst),
        )
    };
    let since = |before: (usize, usize)| {
        let now = calls();
        (now.0 - before.0, now.1 - before.1)
    };
    let git_before = crate::data::git::spawned(&request.repo_path).len();
    let git_since = || crate::data::git::spawned(&request.repo_path)[git_before..].to_vec();
    let calls_before = calls();
    let start = Instant::now();
    let mut first = None;
    let mut ready = std::collections::BTreeSet::new();
    open(data, registry, request, |event| {
        match event {
            OpenEvent::Ready(opened) => {
                if first.is_none() {
                    first = Some((start.elapsed(), git_since(), since(calls_before)));
                }
                ready.insert(opened.source.source_id);
            }
            OpenEvent::Failed(diagnostic) => panic!("{diagnostic:?}"),
            _ => {}
        }
        ControlFlow::Continue(())
    })
    .expect("open");
    assert_eq!(ready.len(), 100, "every layer listed");
    let (first_frame, git_first, calls_first) = first.expect("first frame");
    Measured {
        first_frame,
        every_layer: start.elapsed(),
        git_first,
        git_all: git_since(),
        calls_first,
        calls_all: since(calls_before),
    }
}

/// Open the chain three ways: from the wizard (the stack just resolved), as a saved review in a
/// later run (the stack and listings on disk), and cold (nothing remembered).
fn open_three_ways() -> [Measured; 3] {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).expect("repo dir");
    let oids = chain_repository(&root);
    let provider = Arc::new(ChainProvider {
        oids,
        resolves: AtomicUsize::new(0),
        gets: AtomicUsize::new(0),
    });
    let cache = Arc::new(ReviewCache::open(&dir.path().join("cache.db")));
    let run = |cache: &Arc<ReviewCache>| {
        (
            ReviewData::with_cache(Arc::clone(cache)),
            ProviderRegistry::new()
                .with_cache(Arc::clone(cache))
                .with_source(Arc::clone(&provider) as Arc<dyn ReviewSourceProvider>),
        )
    };

    let (data, registry) = run(&cache);
    let route = registry.sources()[0].clone();
    resolve_through(&route, &root, Some(REPOSITORY.into()), 50, None, Vec::new())
        .map(|response| registry.remember_stack(PROVIDER, REPOSITORY, &response.stack))
        .expect("stack");
    let wizard = measure(&data, &registry, &step_through(&root, 50), &provider);

    let (data, registry) = run(&cache);
    let registry = registry.with_stack_freshness(Duration::ZERO);
    let saved = measure(&data, &registry, &step_through(&root, 73), &provider);

    let (data, registry) = run(&Arc::new(ReviewCache::disabled()));
    let cold = measure(&data, &registry, &step_through(&root, 20), &provider);
    [wizard, saved, cold]
}

fn count(git: &[String], command: &str) -> usize {
    git.iter().filter(|run| run.as_str() == command).count()
}

#[test]
fn a_hundred_layer_step_through_review_asks_little_before_its_first_frame() {
    let [wizard, saved, cold] = open_three_ways();
    // From the wizard: nothing from the provider; one history walk and one diff for the layer;
    // then one diff-tree for every other layer.
    assert_eq!((wizard.calls_first, wizard.calls_all), ((0, 0), (0, 0)));
    assert_eq!(wizard.git_first, ["rev-list", "diff"]);
    assert_eq!(count(&wizard.git_all, "diff-tree"), 1);
    assert_eq!(count(&wizard.git_all, "merge-base"), 0);
    assert!(wizard.git_all.len() <= 5);
    // A saved review: the stack, merge bases, and listings come from disk; only the commits are
    // checked before the first frame, and the stack is confirmed with one request after it.
    assert_eq!((saved.calls_first, saved.calls_all), ((0, 0), (0, 1)));
    assert_eq!(saved.git_first, ["cat-file"]);
    assert!(saved.git_all.len() <= 3);
    assert_eq!(
        count(&saved.git_all, "diff") + count(&saved.git_all, "diff-tree"),
        0
    );
    // Cold: one quick request for the layer, the stack walk only after its first frame.
    assert_eq!((cold.calls_first, cold.calls_all), ((0, 1), (1, 1)));
    assert!(cold.git_first.len() <= 3);
    assert_eq!(count(&cold.git_all, "diff-tree"), 1);
}

fn within(what: &str, took: Duration, budget: Duration) {
    eprintln!("{what}: {took:?} (budget {budget:?})");
    assert!(
        took <= budget * SLACK,
        "{what} took {took:?}, over its budget of {budget:?}"
    );
}

// Timing depends on the machine and on what else runs, so it is run on its own, in release:
// `cargo test --release -p bcode_code_review budgets -- --ignored --nocapture`.
#[test]
#[ignore = "timing budget; run alone in release"]
fn a_hundred_layer_step_through_review_opens_within_its_budgets() {
    let [wizard, saved, cold] = open_three_ways();
    within("wizard: first frame", wizard.first_frame, FIRST_FRAME);
    within("wizard: every layer", wizard.every_layer, EVERY_LAYER);
    within("saved: first frame", saved.first_frame, FIRST_FRAME);
    within(
        "saved: every layer",
        saved.every_layer,
        ROUND_TRIP + EVERY_LAYER / 4,
    );
    // One round trip for the layer; the walk (three) only afterwards.
    within(
        "cold: first frame",
        cold.first_frame,
        ROUND_TRIP + FIRST_FRAME,
    );
    within(
        "cold: every layer",
        cold.every_layer,
        ROUND_TRIP * 4 + EVERY_LAYER,
    );
}
