//! Provider routing, provider-backed resolution, and whole-stack attribution tests.
//!
//! A fake provider serves the fixture's stacked branches (`fixture/stack-1..3`) as pull requests
//! 101..103 and "fetches" them by resolving the local branches, so no network is used.

use super::*;
use crate::data::ReviewData;
use crate::testing::fixture::{
    FixtureSpec, GitFixture, HEAD_BRANCH, STACK_BRANCHES, STACK_SHARED_FILE,
};
use bcode_code_review_models::{
    AnnotationSeverity, BranchAnnotation, BranchLink, OP_REVIEW_DISCOVERY_DISCOVER,
    OP_REVIEW_DISCOVERY_MANIFEST, OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_FETCH,
    OP_REVIEW_SOURCE_PULLS_LIST, OP_REVIEW_SOURCE_STACK_RESOLVE, PullRequestSummary,
    REVIEW_DIAGNOSTIC_PROVIDER_FAILED, REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED,
    REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION, REVIEW_SOURCE_ERROR_UNAUTHENTICATED,
    ResolveReviewSourcesResponse, ResolvedReviewStack, ResolvedStackLayer, ReviewDiscoveryManifest,
    ReviewDiscoveryRequest, ReviewDiscoveryResponse, ReviewFileHunksRequest, ReviewLineKind,
    ReviewSourceBatchCapabilities, ReviewSourceCapabilities, ReviewSourcePullsFetchResponse,
    ReviewSourcePullsListResponse, ReviewStackAttributionRequest, ReviewStackAttributionResponse,
    ReviewStackLayer, ReviewStackLineAttribution, RevisionRef,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

const FAKE_PROVIDER: &str = "fake";
const FAKE_REPOSITORY: &str = "octo/fixture";
/// Pull request numbers of the fixture's stack layers, bottom to top.
const LAYER_NUMBERS: [u64; 3] = [101, 102, 103];

fn pull(number: u64, base: &str, head: &str) -> PullRequestSummary {
    PullRequestSummary {
        number,
        title: format!("Layer {number}"),
        author: Some("octocat".into()),
        base_branch: base.into(),
        head_branch: head.into(),
        head_oid: String::new(),
        base_oid: None,
        review_requested_from_me: false,
        draft: false,
        updated: None,
        url: None,
    }
}

fn fixture_stack(requested: u64) -> ReviewStack {
    let bases = [HEAD_BRANCH, STACK_BRANCHES[0], STACK_BRANCHES[1]];
    ReviewStack {
        trunk: HEAD_BRANCH.into(),
        rooted_at_default_branch: true,
        layers: LAYER_NUMBERS
            .iter()
            .zip(bases.iter().zip(STACK_BRANCHES))
            .enumerate()
            .map(|(index, (number, (base, head)))| ReviewStackLayer {
                pull: pull(*number, base, head),
                parent: index.checked_sub(1).map(|below| LAYER_NUMBERS[below]),
            })
            .collect(),
        requested,
        siblings: Vec::new(),
        warnings: Vec::new(),
    }
}

fn rev_parse(repo: &Path, spec: &str) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", spec])
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(output.status.success(), "rev-parse {spec}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A provider that serves the fixture stack and fetches by resolving local branches. It answers
/// JSON operations (the shape a plugin-backed provider has) and records every call; [`registry`]
/// registers it through the JSON adapters.
#[derive(Clone)]
struct FakeProvider {
    repo: PathBuf,
    /// Whether the provider is registered at all.
    enabled: bool,
    /// Error returned by every fetch, when set.
    fetch_error: Option<ProviderCallError>,
    /// Whether the provider fetches many pull requests in one call.
    batch_fetch: bool,
    /// What a discovery contributor answers, when one is enabled.
    discovery: Option<ReviewDiscoveryResponse>,
    /// Calls received, as `operation` or `operation #number base`.
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeProvider {
    /// "Fetch" pull request `number` by resolving the fixture's local branches.
    fn fetched(&self, number: u64, base_branch: Option<String>) -> ReviewPullFetchResult {
        let layer = fixture_stack(number)
            .layers
            .into_iter()
            .find(|layer| layer.pull.number == number)
            .expect("known pull request");
        let base_branch = base_branch.unwrap_or(layer.pull.base_branch);
        ReviewPullFetchResult {
            number,
            head_ref: format!("refs/heads/{}", layer.pull.head_branch),
            head_oid: rev_parse(&self.repo, &layer.pull.head_branch),
            base_ref: format!("refs/heads/{base_branch}"),
            base_oid: rev_parse(&self.repo, &base_branch),
            base_branch,
        }
    }

    fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            enabled: true,
            fetch_error: None,
            batch_fetch: false,
            discovery: None,
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    /// Answer `review_source.pulls.list` lookups by number from the fixture stack.
    fn listed(&self, payload: &[u8]) -> ReviewSourcePullsListResponse {
        let request: ReviewSourcePullsListRequest =
            serde_json::from_slice(payload).expect("pulls request");
        let query = request.query.unwrap_or_default();
        self.calls
            .lock()
            .expect("calls")
            .push(format!("{OP_REVIEW_SOURCE_PULLS_LIST} {query}"));
        ReviewSourcePullsListResponse {
            repository: FAKE_REPOSITORY.into(),
            default_branch: Some(HEAD_BRANCH.into()),
            pulls: fixture_stack(103)
                .layers
                .into_iter()
                .map(|layer| layer.pull)
                .filter(|pull| pull.number.to_string() == query)
                .collect(),
        }
    }

    /// Answer the discovery contributor's operations.
    fn discovered(&self, operation: &str, payload: &[u8]) -> Vec<u8> {
        match operation {
            OP_REVIEW_DISCOVERY_MANIFEST => serde_json::to_vec(&ReviewDiscoveryManifest {
                id: "fake".into(),
                label: "Fake tool".into(),
            }),
            OP_REVIEW_DISCOVERY_DISCOVER => {
                let request: ReviewDiscoveryRequest =
                    serde_json::from_slice(payload).expect("discovery request");
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("{operation} {}", request.branches.join(" ")));
                serde_json::to_vec(self.discovery.as_ref().expect("discovery enabled"))
            }
            other => panic!("unexpected discovery operation {other}"),
        }
        .expect("encode")
    }

    fn answer(&self, operation: &str, payload: &[u8]) -> Result<Vec<u8>, ProviderCallError> {
        let encode = |value: &dyn erased::Encode| Ok(value.encode());
        match operation {
            OP_REVIEW_SOURCE_MANIFEST => {
                self.calls.lock().expect("calls").push(operation.into());
                encode(&ReviewSourceProviderManifest {
                    id: FAKE_PROVIDER.into(),
                    label: "Fake".into(),
                    capabilities: ReviewSourceCapabilities {
                        pulls_list: false,
                        stack_resolve: true,
                        pull_fetch: true,
                        batched: ReviewSourceBatchCapabilities {
                            pulls_fetch: self.batch_fetch,
                            pulls_get: false,
                        },
                    },
                })
            }
            OP_REVIEW_SOURCE_STACK_RESOLVE => {
                let request: ReviewSourceStackResolveRequest =
                    serde_json::from_slice(payload).expect("stack request");
                assert_eq!(request.repository.as_deref(), Some(FAKE_REPOSITORY));
                self.calls
                    .lock()
                    .expect("calls")
                    .push(if request.hints.is_empty() {
                        format!("{operation} #{}", request.number)
                    } else {
                        format!(
                            "{operation} #{} hints {}",
                            request.number,
                            request.hints.join(" ")
                        )
                    });
                encode(&ReviewSourceStackResolveResponse {
                    repository: FAKE_REPOSITORY.into(),
                    stack: fixture_stack(request.number),
                })
            }
            OP_REVIEW_SOURCE_PULL_FETCH => {
                let request: ReviewSourcePullFetchRequest =
                    serde_json::from_slice(payload).expect("fetch request");
                self.calls.lock().expect("calls").push(format!(
                    "{operation} #{} {}",
                    request.number,
                    request.base_branch.as_deref().unwrap_or("-")
                ));
                if let Some(error) = &self.fetch_error {
                    return Err(error.clone());
                }
                encode(&self.fetched(request.number, request.base_branch))
            }
            OP_REVIEW_SOURCE_PULLS_LIST => encode(&self.listed(payload)),
            OP_REVIEW_SOURCE_PULLS_FETCH => {
                assert!(
                    self.batch_fetch,
                    "batched fetch used without the capability"
                );
                let request: ReviewSourcePullsFetchRequest =
                    serde_json::from_slice(payload).expect("fetch request");
                let numbers: Vec<String> = request
                    .pulls
                    .iter()
                    .map(|pull| format!("#{}", pull.number))
                    .collect();
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("{operation} {}", numbers.join(" ")));
                let fetched = request
                    .pulls
                    .iter()
                    .map(|target| self.fetched(target.number, Some(target.base_branch.clone())))
                    .collect();
                encode(&ReviewSourcePullsFetchResponse { fetched })
            }
            other => panic!("unexpected operation {other}"),
        }
    }
}

/// Object-safe JSON encoding for the fake's heterogeneous answers.
mod erased {
    pub trait Encode {
        fn encode(&self) -> Vec<u8>;
    }

    impl<T: serde::Serialize> Encode for T {
        fn encode(&self) -> Vec<u8> {
            serde_json::to_vec(self).expect("encode")
        }
    }
}

/// A registry with `fake` as its provider (when enabled) and its discovery contributor (when it
/// has an answer), both reached through the JSON adapters.
fn registry(fake: &FakeProvider) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    if fake.enabled {
        let served = fake.clone();
        let manifest: ReviewSourceProviderManifest = serde_json::from_slice(
            &fake
                .answer(OP_REVIEW_SOURCE_MANIFEST, b"{}")
                .expect("manifest"),
        )
        .expect("manifest");
        registry.add_source(Arc::new(JsonReviewSource::new(
            manifest,
            Box::new(move |operation, payload| served.answer(operation, &payload)),
        )));
    }
    if fake.discovery.is_some() {
        let served = fake.clone();
        registry.add_contributor(Arc::new(JsonDiscovery::new(
            ReviewDiscoveryManifest {
                id: "fake".into(),
                label: "Fake tool".into(),
            },
            Box::new(move |operation, payload| Ok(served.discovered(operation, &payload))),
        )));
    }
    registry
}

fn source(id: &str, kind: ReviewSourceKind) -> ReviewSource {
    ReviewSource {
        id: id.into(),
        label: kind.label(),
        kind,
        included: true,
    }
}

fn layer_source(id: &str, number: u64) -> ReviewSource {
    source(
        id,
        ReviewSourceKind::StackLayer {
            provider: FAKE_PROVIDER.into(),
            repository: FAKE_REPOSITORY.into(),
            number,
        },
    )
}

fn stack_source(id: &str, top: u64) -> ReviewSource {
    source(
        id,
        ReviewSourceKind::Stack {
            provider: FAKE_PROVIDER.into(),
            repository: FAKE_REPOSITORY.into(),
            top,
        },
    )
}

fn pull_source(id: &str, number: u64) -> ReviewSource {
    source(
        id,
        ReviewSourceKind::PullRequest {
            provider: FAKE_PROVIDER.into(),
            repository: FAKE_REPOSITORY.into(),
            number,
        },
    )
}

/// Resolve `sources` the way a client does: ask the providers, then the service.
fn resolve(
    provider: &FakeProvider,
    repo: &Path,
    sources: Vec<ReviewSource>,
) -> ResolveReviewSourcesResponse {
    let request = resolve_request_with_providers(&registry(provider), repo, sources, None);
    // The request crosses the service boundary as JSON.
    let request: ResolveReviewSourcesRequest =
        serde_json::from_slice(&serde_json::to_vec(&request).expect("encode")).expect("decode");
    ReviewData::default()
        .resolve_sources(&request)
        .expect("resolve")
}

fn pair(response: &ResolveReviewSourcesResponse, id: &str) -> (RevisionRef, RevisionRef) {
    let source = response
        .sources
        .iter()
        .find(|source| source.source_id == id)
        .unwrap_or_else(|| panic!("{id} unresolved: {:?}", response.diagnostics));
    (source.base.clone(), source.head.clone())
}

fn commit(repo: &Path, spec: &str) -> RevisionRef {
    RevisionRef::commit(rev_parse(repo, spec))
}

#[test]
fn stack_and_pull_request_sources_resolve_through_a_provider() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let provider = FakeProvider::new(repo);
    let response = resolve(
        &provider,
        repo,
        vec![
            layer_source("layer-1", 101),
            layer_source("layer-2", 102),
            stack_source("stack-3", 103),
            stack_source("stack-2", 102),
            pull_source("pr-3", 103),
            source("unstaged", ReviewSourceKind::WorkingTreeUnstaged),
        ],
    );
    assert!(
        response.diagnostics.is_empty(),
        "{:?}",
        response.diagnostics
    );
    let [stack_1, stack_2, stack_3] = STACK_BRANCHES.map(|branch| commit(repo, branch));
    let main = commit(repo, HEAD_BRANCH);
    // A layer is reviewed against the layer below it (the trunk for the bottom layer).
    assert_eq!(pair(&response, "layer-1"), (main.clone(), stack_1.clone()));
    assert_eq!(
        pair(&response, "layer-2"),
        (stack_1.clone(), stack_2.clone())
    );
    // A whole stack runs from the trunk merge base to the top (or the chosen top layer).
    assert_eq!(pair(&response, "stack-3"), (main.clone(), stack_3.clone()));
    assert_eq!(pair(&response, "stack-2"), (main.clone(), stack_2.clone()));
    // A plain pull request is its own diff against its base branch.
    assert_eq!(pair(&response, "pr-3"), (stack_2.clone(), stack_3.clone()));
    assert_eq!(
        pair(&response, "unstaged"),
        (RevisionRef::Index, RevisionRef::WorkingTree)
    );

    // Stack sources also report their pinned layers.
    let stacks: Vec<(&str, Vec<u64>)> = response
        .stacks
        .iter()
        .map(|stack| {
            (
                stack.source_id.as_str(),
                stack.layers.iter().map(|layer| layer.number).collect(),
            )
        })
        .collect();
    assert_eq!(
        stacks,
        vec![
            ("layer-1", vec![101]),
            ("layer-2", vec![101, 102]),
            ("stack-3", vec![101, 102, 103]),
            ("stack-2", vec![101, 102]),
        ]
    );
    let whole = &response.stacks[2];
    assert_eq!(whole.trunk, HEAD_BRANCH);
    assert_eq!(RevisionRef::commit(whole.trunk_oid.clone()), main);
    assert_eq!(RevisionRef::commit(whole.base_oid.clone()), main);
    assert_eq!(whole.top_oid().map(RevisionRef::commit), Some(stack_3));
    let layer_bases: Vec<RevisionRef> = whole
        .layers
        .iter()
        .map(|layer| RevisionRef::commit(layer.base_oid.clone()))
        .collect();
    assert_eq!(layer_bases, vec![main, stack_1, stack_2]);

    // Each stack layer was fetched with its own base branch.
    let calls = provider.calls();
    assert!(calls.contains(&format!(
        "{OP_REVIEW_SOURCE_PULL_FETCH} #102 {}",
        STACK_BRANCHES[0]
    )));
    assert!(calls.contains(&format!("{OP_REVIEW_SOURCE_PULL_FETCH} #101 {HEAD_BRANCH}")));
    assert!(calls.contains(&format!("{OP_REVIEW_SOURCE_PULL_FETCH} #103 -")));
}

#[test]
fn one_open_resolves_each_stack_once_and_fetches_each_pull_request_once() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    // "Step through every layer" plus the whole stack: four sources over one three-layer stack.
    let sources = || {
        vec![
            layer_source("layer-1", 101),
            layer_source("layer-2", 102),
            layer_source("layer-3", 103),
            stack_source("stack", 103),
        ]
    };
    for batch_fetch in [false, true] {
        let mut provider = FakeProvider::new(repo);
        provider.batch_fetch = batch_fetch;
        let response = resolve(&provider, repo, sources());
        assert!(
            response.diagnostics.is_empty(),
            "{:?}",
            response.diagnostics
        );
        assert_eq!(response.stacks.len(), 4);
        let calls = provider.calls();
        let count = |prefix: &str| calls.iter().filter(|call| call.starts_with(prefix)).count();
        assert_eq!(count(OP_REVIEW_SOURCE_STACK_RESOLVE), 1, "{calls:?}");
        if batch_fetch {
            // Every layer in one call, planned across all four sources.
            assert_eq!(count(OP_REVIEW_SOURCE_PULL_FETCH), 0, "{calls:?}");
            assert!(
                calls.contains(&format!("{OP_REVIEW_SOURCE_PULLS_FETCH} #101 #102 #103")),
                "{calls:?}"
            );
            assert_eq!(count(OP_REVIEW_SOURCE_PULLS_FETCH), 1, "{calls:?}");
        } else {
            assert_eq!(count(OP_REVIEW_SOURCE_PULL_FETCH), 3, "{calls:?}");
        }
    }
}

#[test]
fn discovery_hints_go_to_the_provider_and_its_warnings_onto_the_stack() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut provider = FakeProvider::new(repo);
    let link = |branch: &str, parent: &str| BranchLink {
        branch: branch.into(),
        parent: parent.into(),
        parent_oid: None,
    };
    provider.discovery = Some(ReviewDiscoveryResponse {
        // The tool records stack-2's parent as main, unlike its pull request (stack-1).
        links: vec![
            link(STACK_BRANCHES[0], HEAD_BRANCH),
            link(STACK_BRANCHES[1], HEAD_BRANCH),
            link(STACK_BRANCHES[2], STACK_BRANCHES[1]),
        ],
        related: Vec::new(),
        annotations: vec![BranchAnnotation {
            branch: STACK_BRANCHES[2].into(),
            severity: AnnotationSeverity::Warning,
            code: "needs_restack".into(),
            message: "needs a restack".into(),
        }],
    });
    let registry = registry(&provider);
    let response = crate::discovery::resolve_stack_with_discovery(
        &registry,
        &registry.sources()[0],
        repo,
        Some(FAKE_REPOSITORY.into()),
        103,
        None,
        None,
    )
    .expect("stack");
    let calls = provider.calls();
    // The head branch is looked up by number, the contributor asked about it, and the provider
    // given its hints.
    assert!(
        calls.contains(&format!("{OP_REVIEW_SOURCE_PULLS_LIST} 103")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!(
            "{OP_REVIEW_DISCOVERY_DISCOVER} {}",
            STACK_BRANCHES[2]
        )),
        "{calls:?}"
    );
    let resolve = calls
        .iter()
        .find(|call| call.starts_with(OP_REVIEW_SOURCE_STACK_RESOLVE))
        .expect("resolved");
    // Hints follow the tool's links up to the trunk and no further: its wrong parent for
    // stack-2 leaves stack-1 out, which costs the provider a lookup, not a wrong stack.
    assert_eq!(
        resolve,
        &format!(
            "{OP_REVIEW_SOURCE_STACK_RESOLVE} #103 hints {} {HEAD_BRANCH}",
            STACK_BRANCHES[1]
        )
    );
    assert_eq!(
        response.stack.warnings,
        [
            format!(
                "Fake tool records {HEAD_BRANCH} as the parent of #102, but its pull request's \
                 base is {}",
                STACK_BRANCHES[0]
            ),
            "Fake tool: #103 needs a restack".to_owned(),
        ]
    );
}

#[test]
fn a_pull_request_a_provider_reported_before_is_not_looked_up_again_for_discovery() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut provider = FakeProvider::new(repo);
    provider.discovery = Some(ReviewDiscoveryResponse::default());
    let registry = registry(&provider);
    let route = &registry.sources()[0];
    // A listing (the home screen's, say) reported #103 and its head branch.
    registry.note_pulls(
        FAKE_PROVIDER,
        FAKE_REPOSITORY,
        &[pull(103, STACK_BRANCHES[1], STACK_BRANCHES[2])],
    );
    assert_eq!(
        registry
            .known_pull(FAKE_PROVIDER, FAKE_REPOSITORY, 103)
            .map(|pull| pull.head_branch),
        Some(STACK_BRANCHES[2].to_owned())
    );
    assert_eq!(registry.known_pull(FAKE_PROVIDER, "other/repo", 103), None);
    crate::discovery::resolve_stack_with_discovery(
        &registry,
        route,
        repo,
        Some(FAKE_REPOSITORY.into()),
        103,
        None,
        None,
    )
    .expect("stack");
    let calls = provider.calls();
    // Discovery starts from the noted head branch without asking the provider for it.
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with(OP_REVIEW_SOURCE_PULLS_LIST)),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!(
            "{OP_REVIEW_DISCOVERY_DISCOVER} {}",
            STACK_BRANCHES[2]
        )),
        "{calls:?}"
    );
    // The resolved stack's layers are noted too.
    assert!(
        registry
            .known_pull(FAKE_PROVIDER, FAKE_REPOSITORY, 101)
            .is_some()
    );
}

#[test]
fn provider_backed_sources_report_provider_required_when_no_provider_is_enabled() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    // The provider plugin is disabled: nothing serves `bcode.review_source/v1`.
    let mut provider = FakeProvider::new(repo);
    provider.enabled = false;
    let mut other = pull_source("other-provider", 103);
    other.kind = ReviewSourceKind::PullRequest {
        provider: "someone-else".into(),
        repository: FAKE_REPOSITORY.into(),
        number: 103,
    };
    let response = resolve(
        &provider,
        repo,
        vec![
            pull_source("pr", 101),
            layer_source("layer", 102),
            stack_source("stack", 103),
            source("last", ReviewSourceKind::LastCommit),
            source(
                "branch",
                ReviewSourceKind::CurrentBranch {
                    base: Some(STACK_BRANCHES[0].into()),
                },
            ),
        ],
    );
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
        ]
    );
    // Local sources keep working.
    let resolved: Vec<&str> = response
        .sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect();
    assert_eq!(resolved, vec!["last", "branch"]);
    assert_eq!(response.stacks.len(), 0);
    assert_eq!(provider.calls().len(), 0);

    // An enabled provider with a different id does not answer for this one.
    let enabled = FakeProvider::new(repo);
    let response = resolve(&enabled, repo, vec![other]);
    assert_eq!(
        response.diagnostics[0].code,
        REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED
    );
    assert_eq!(enabled.calls(), vec![OP_REVIEW_SOURCE_MANIFEST.to_string()]);

    // Workspaces without provider-backed sources never ask providers anything.
    let registry = registry(&enabled);
    let before = enabled.calls().len();
    let request = resolve_request_with_providers(
        &registry,
        repo,
        vec![source("last", ReviewSourceKind::LastCommit)],
        None,
    );
    assert_eq!(request.provider_resolutions.len(), 0);
    assert_eq!(enabled.calls().len(), before);
}

#[test]
fn provider_failures_and_missing_commits_become_diagnostics() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut provider = FakeProvider::new(repo);
    provider.fetch_error = Some(ProviderCallError {
        code: REVIEW_SOURCE_ERROR_UNAUTHENTICATED.into(),
        message: "no token".into(),
    });
    let response = resolve(
        &provider,
        repo,
        vec![
            stack_source("stack", 103),
            source("last", ReviewSourceKind::LastCommit),
        ],
    );
    assert_eq!(response.diagnostics.len(), 1);
    let diagnostic = &response.diagnostics[0];
    assert_eq!(diagnostic.code, REVIEW_DIAGNOSTIC_PROVIDER_FAILED);
    assert!(
        diagnostic.message.contains("no token"),
        "{}",
        diagnostic.message
    );
    assert_eq!(response.sources.len(), 1);

    // A fetch result naming commits that are not local is reported, not trusted.
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let response = ReviewData::default()
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![pull_source("pr", 103)],
            provider_resolutions: vec![ReviewSourceProviderResolution {
                source_id: "pr".into(),
                outcome: ReviewSourceProviderOutcome::Resolved {
                    stack: None,
                    fetched: vec![ReviewPullFetchResult {
                        number: 103,
                        head_ref: "refs/x".into(),
                        head_oid: missing.into(),
                        base_branch: HEAD_BRANCH.into(),
                        base_ref: "refs/y".into(),
                        base_oid: rev_parse(repo, HEAD_BRANCH),
                    }],
                },
            }],
        })
        .expect("resolve");
    assert_eq!(
        response.diagnostics[0].code,
        REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION
    );
    // Stack sources need the provider's stack.
    let response = ReviewData::default()
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![layer_source("layer", 102)],
            provider_resolutions: vec![ReviewSourceProviderResolution {
                source_id: "layer".into(),
                outcome: ReviewSourceProviderOutcome::Resolved {
                    stack: None,
                    fetched: Vec::new(),
                },
            }],
        })
        .expect("resolve");
    assert_eq!(
        response.diagnostics[0].code,
        REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION
    );
}

fn whole_stack(repo: &Path) -> ResolvedReviewStack {
    let provider = FakeProvider::new(repo);
    let mut response = resolve(&provider, repo, vec![stack_source("stack", 103)]);
    assert!(
        response.diagnostics.is_empty(),
        "{:?}",
        response.diagnostics
    );
    response.stacks.remove(0)
}

fn attribute(
    service: &ReviewData,
    repo: &Path,
    stack: &ResolvedReviewStack,
    path: &str,
    old_path: Option<&str>,
) -> ReviewStackAttributionResponse {
    service
        .stack_attribution(&ReviewStackAttributionRequest {
            repo_path: repo.to_path_buf(),
            stack: stack.clone(),
            path: path.into(),
            old_path: old_path.map(str::to_string),
        })
        .expect("attribution")
}

#[test]
fn whole_stack_attribution_labels_each_layers_region_of_the_shared_file() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let stack = whole_stack(repo);
    let service = ReviewData::default();
    let attribution = attribute(&service, repo, &stack, STACK_SHARED_FILE, None);
    assert!(!attribution.binary);

    // New side: each line tagged by a layer belongs to that layer; original lines to none.
    let top = fixture.git(&[
        "show",
        &format!("{}:{STACK_SHARED_FILE}", STACK_BRANCHES[2]),
    ]);
    for (index, text) in top.lines().enumerate() {
        let line = u32::try_from(index + 1).expect("line");
        let expected = (1..=3).find_map(|layer| {
            text.contains(&format!("// layer{layer} line"))
                .then(|| LAYER_NUMBERS[layer - 1])
        });
        assert_eq!(
            attribution.new_line_layer(line),
            expected,
            "line {line}: {text}"
        );
    }
    let layers: BTreeSet<u64> = attribution
        .new_ranges
        .iter()
        .map(|range| range.layer)
        .collect();
    assert_eq!(layers, LAYER_NUMBERS.into_iter().collect());

    // Old side: every line the whole-stack diff removes was removed by the layer that wrote
    // its replacement in the same hunk.
    let hunks = service
        .file_hunks_get(&ReviewFileHunksRequest {
            repo_path: repo.to_path_buf(),
            source: bcode_code_review_models::ResolvedSource {
                source_id: "stack".into(),
                base: RevisionRef::commit(stack.base_oid.clone()),
                head: RevisionRef::commit(stack.top_oid().expect("top").to_string()),
                mode: bcode_code_review_models::ResolvedSourceMode::Diff,
                paths: Vec::new(),
                line_range: None,
                part: None,
            },
            path: STACK_SHARED_FILE.into(),
            old_path: None,
            context_lines: Some(0),
            ignore_whitespace: false,
            max_lines: None,
        })
        .expect("hunks")
        .hunks;
    assert_eq!(hunks.len(), 3);
    for hunk in &hunks {
        let added: BTreeSet<Option<u64>> = hunk
            .lines
            .iter()
            .filter(|line| line.kind == ReviewLineKind::Added)
            .map(|line| attribution.new_line_layer(line.new_line.expect("new line")))
            .collect();
        let removed: BTreeSet<Option<u64>> = hunk
            .lines
            .iter()
            .filter(|line| line.kind == ReviewLineKind::Removed)
            .map(|line| attribution.old_line_layer(line.old_line.expect("old line")))
            .collect();
        assert_eq!(added.len(), 1, "{added:?}");
        assert_eq!(added, removed);
        assert!(added.iter().all(Option::is_some));
    }
    let removed_lines: u32 = attribution
        .old_ranges
        .iter()
        .map(|range| range.end - range.start + 1)
        .sum();
    assert_eq!(removed_lines, 15);

    // A file added by one layer is entirely that layer's.
    let added = attribute(
        &service,
        repo,
        &stack,
        &GitFixture::stack_layer_path(2),
        None,
    );
    assert_eq!(
        added.new_ranges,
        vec![ReviewStackLineAttribution {
            start: 1,
            end: 20,
            layer: 102,
            layer_line: Some(1),
        }]
    );
    assert_eq!(added.old_ranges.len(), 0);

    // A file the stack does not touch has no attribution.
    let untouched = attribute(&service, repo, &stack, &GitFixture::modified_path(0), None);
    assert!(untouched.new_ranges.is_empty() && untouched.old_ranges.is_empty());
}

#[test]
fn attribution_of_the_top_layer_rejects_unconfined_paths_and_empty_stacks() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let stack = whole_stack(repo);
    let attribution = ReviewData::default()
        .stack_attribution(&ReviewStackAttributionRequest {
            repo_path: repo.to_path_buf(),
            stack: stack.clone(),
            path: GitFixture::stack_layer_path(3),
            old_path: None,
        })
        .expect("attribution");
    assert_eq!(attribution.new_line_layer(1), Some(103));

    // Unconfined paths and malformed stacks are rejected.
    let mut bad = ReviewStackAttributionRequest {
        repo_path: repo.to_path_buf(),
        stack,
        path: "../outside".into(),
        old_path: None,
    };
    assert!(ReviewData::default().stack_attribution(&bad).is_err());
    bad.path = STACK_SHARED_FILE.into();
    bad.stack.layers.clear();
    assert!(ReviewData::default().stack_attribution(&bad).is_err());
}

fn git(dir: &Path, args: &[&str]) -> String {
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

#[test]
fn attribution_follows_renames_and_maps_lines_through_later_layers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = std::fs::canonicalize(dir.path()).expect("canonical");
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    let lines = |values: &[&str]| values.join("\n") + "\n";
    let original = [
        "fn one() {}",
        "fn two() {}",
        "fn three() {}",
        "fn four() {}",
        "fn five() {}",
        "fn six() {}",
        "fn seven() {}",
        "fn eight() {}",
    ];
    std::fs::write(repo.join("f.rs"), lines(&original)).expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    // Layer 1 renames the file and rewrites line 3.
    git(&repo, &["mv", "f.rs", "g.rs"]);
    let mut layer_1 = original.to_vec();
    layer_1[2] = "fn three_v1() {}";
    std::fs::write(repo.join("g.rs"), lines(&layer_1)).expect("write");
    git(&repo, &["commit", "--quiet", "-am", "layer 1"]);
    let head_1 = git(&repo, &["rev-parse", "HEAD"]);
    // Layer 2 deletes line 1 and inserts two lines after `five` (shifting everything).
    let mut layer_2: Vec<&str> = layer_1[1..].to_vec();
    layer_2.splice(4..4, ["fn added_a() {}", "fn added_b() {}"]);
    std::fs::write(repo.join("g.rs"), lines(&layer_2)).expect("write");
    git(&repo, &["commit", "--quiet", "-am", "layer 2"]);
    let head_2 = git(&repo, &["rev-parse", "HEAD"]);

    let stack = ResolvedReviewStack {
        source_id: "stack".into(),
        trunk: "main".into(),
        trunk_oid: base.clone(),
        base_oid: base.clone(),
        layers: vec![
            ResolvedStackLayer {
                number: 1,
                title: "one".into(),
                base_oid: base,
                head_oid: head_1.clone(),
                base_branch: String::new(),
                head_branch: String::new(),
            },
            ResolvedStackLayer {
                number: 2,
                title: "two".into(),
                base_oid: head_1,
                head_oid: head_2,
                base_branch: String::new(),
                head_branch: String::new(),
            },
        ],
    };
    let attribution = attribute(&ReviewData::default(), &repo, &stack, "g.rs", Some("f.rs"));
    let range = |start, end, layer, layer_line| ReviewStackLineAttribution {
        start,
        end,
        layer,
        layer_line: Some(layer_line),
    };
    // Top file: two, three_v1 (layer 1), four, five, added_a, added_b (layer 2), six, seven,
    // eight. `three_v1` is line 3 of layer 1's head (layer 2 later removed line 1 above it);
    // the added lines are lines 5 and 6 of layer 2's head.
    assert_eq!(
        attribution.new_ranges,
        vec![range(2, 2, 1, 3), range(5, 6, 2, 5)]
    );
    assert_eq!(attribution.new_line_origin(2), Some((1, Some(3))));
    assert_eq!(attribution.new_line_origin(6), Some((2, Some(6))));
    // Base file: line 1 removed by layer 2 (line 1 of layer 1's head), line 3 replaced by
    // layer 1 (line 3 of the base).
    assert_eq!(
        attribution.old_ranges,
        vec![range(1, 1, 2, 1), range(3, 3, 1, 3)]
    );
}
