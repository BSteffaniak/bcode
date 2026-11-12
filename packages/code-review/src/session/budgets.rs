//! Latency budgets of opening a step-through review of a 100-layer stack (see "Performance
//! budgets" in `docs/review-loading-architecture.md`).
//!
//! A synthetic repository holds the stack's commits locally; a provider answers about it with a
//! fixed delay per round trip, as a forge would, and fetches locally (as GitHub's provider does
//! when the commits are already there). What each path may ask the provider is asserted exactly;
//! how long it takes is asserted against the budgets.

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
        })
    }

    fn resolve_stack(
        &self,
        request: &ReviewSourceStackResolveRequest,
    ) -> ProviderResult<ReviewSourceStackResolveResponse> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(ROUND_TRIP);
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
    }
}

/// How long until the first frame and until every layer is listed.
struct Timing {
    first_frame: Duration,
    every_layer: Duration,
}

fn timed_open(data: &ReviewData, registry: &ProviderRegistry, request: &OpenRequest) -> Timing {
    let start = Instant::now();
    let mut first_frame = None;
    let mut every_layer = None;
    open(data, registry, request, |event| {
        match event {
            OpenEvent::Ready(ready) => {
                assert_eq!(ready.ready.len(), 1);
                assert_eq!(ready.pending.len(), 99, "the rest of the stack, pending");
                first_frame = Some(start.elapsed());
            }
            OpenEvent::Rest(rest) => {
                assert_eq!(rest.ready.len(), 100, "{:?}", rest.diagnostics);
                every_layer = Some(start.elapsed());
            }
            OpenEvent::Stage(_) => {}
        }
        ControlFlow::Continue(())
    })
    .expect("open");
    Timing {
        first_frame: first_frame.expect("first frame"),
        every_layer: every_layer.expect("every layer"),
    }
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
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).expect("repo dir");
    let oids = chain_repository(&root);
    let cache = Arc::new(ReviewCache::open(&dir.path().join("cache.db")));
    let provider = Arc::new(ChainProvider {
        oids,
        resolves: AtomicUsize::new(0),
        gets: AtomicUsize::new(0),
    });
    let calls = || {
        (
            provider.resolves.load(Ordering::SeqCst),
            provider.gets.load(Ordering::SeqCst),
        )
    };
    let process = || {
        (
            ReviewData::with_cache(Arc::clone(&cache)),
            ProviderRegistry::new()
                .with_cache(Arc::clone(&cache))
                .with_source(Arc::clone(&provider) as Arc<dyn ReviewSourceProvider>),
        )
    };

    // From the wizard: the stack was just resolved (one round trip), so opening asks nothing
    // before its first frame.
    let (data, registry) = process();
    let route = registry.sources()[0].clone();
    resolve_through(&route, &root, Some(REPOSITORY.into()), 50, None, Vec::new())
        .map(|response| registry.remember_stack(PROVIDER, REPOSITORY, &response.stack))
        .expect("stack");
    assert_eq!(calls(), (1, 0));
    let wizard = timed_open(&data, &registry, &step_through(&root, 50));
    assert_eq!(
        calls(),
        (1, 0),
        "a fresh stack is neither resolved nor confirmed"
    );
    within("wizard: first frame", wizard.first_frame, FIRST_FRAME);
    within("wizard: every layer", wizard.every_layer, EVERY_LAYER);

    // A saved review reopened by a later run: the stack and every listing come from disk; the
    // stack is confirmed with one round trip, after the first frame.
    let (data, registry) = process();
    let registry = registry.with_stack_freshness(Duration::ZERO);
    let saved = timed_open(&data, &registry, &step_through(&root, 73));
    assert_eq!(calls(), (1, 1), "one confirmation, no resolution");
    within("saved: first frame", saved.first_frame, FIRST_FRAME);
    within(
        "saved: every layer",
        saved.every_layer,
        ROUND_TRIP + EVERY_LAYER / 4,
    );

    // Cold, nothing remembered: the requested layer after one round trip.
    let cold_cache = Arc::new(ReviewCache::disabled());
    let data = ReviewData::with_cache(Arc::clone(&cold_cache));
    let registry = ProviderRegistry::new()
        .with_cache(cold_cache)
        .with_source(Arc::clone(&provider) as Arc<dyn ReviewSourceProvider>);
    let cold = timed_open(&data, &registry, &step_through(&root, 20));
    assert_eq!(calls(), (2, 1), "one resolution for the whole stack");
    within(
        "cold: first frame",
        cold.first_frame,
        ROUND_TRIP + FIRST_FRAME,
    );
    within(
        "cold: every layer",
        cold.every_layer,
        ROUND_TRIP + EVERY_LAYER,
    );
}
