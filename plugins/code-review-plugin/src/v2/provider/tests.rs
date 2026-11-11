//! Provider routing, provider-backed resolution, and whole-stack attribution tests.
//!
//! A fake provider serves the fixture's stacked branches (`fixture/stack-1..3`) as pull requests
//! 101..103 and "fetches" them by resolving the local branches, so no network is used.

use super::*;
use crate::v2::service::V2Service;
use crate::v2::testing::fixture::{
    FixtureSpec, GitFixture, HEAD_BRANCH, STACK_BRANCHES, STACK_SHARED_FILE,
};
use bcode_code_review_models::{
    OP_REVIEW_STACK_ATTRIBUTION, PullRequestSummary, REVIEW_DIAGNOSTIC_PROVIDER_FAILED,
    REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED, REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION,
    REVIEW_SOURCE_ERROR_UNAUTHENTICATED, ResolveReviewSourcesResponse, ResolvedReviewStack,
    ResolvedStackLayer, ReviewFileHunksRequest, ReviewLineKind, ReviewSourceCapabilities,
    ReviewStackAttributionRequest, ReviewStackAttributionResponse, ReviewStackLayer,
    ReviewStackLineAttribution, RevisionRef,
};
use bcode_plugin_sdk::prelude::ServiceRequest;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

const FAKE_PLUGIN: &str = "test.fake_review_source";
const FAKE_PROVIDER: &str = "fake";
const FAKE_REPOSITORY: &str = "octo/fixture";
/// Pull request numbers of the fixture's stack layers, bottom to top.
const LAYER_NUMBERS: [u64; 3] = [101, 102, 103];

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(future)
}

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

/// A provider that serves the fixture stack and fetches by resolving local branches.
struct FakeProvider {
    repo: PathBuf,
    /// Plugins advertising the review-source interface (empty: the provider plugin is disabled).
    plugins: Vec<String>,
    /// Error returned by every fetch, when set.
    fetch_error: Option<ProviderCallError>,
    /// Calls received, as `operation` or `operation #number base`.
    calls: Mutex<Vec<String>>,
}

impl FakeProvider {
    fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            plugins: vec![FAKE_PLUGIN.to_string()],
            fetch_error: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
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
                    .push(format!("{operation} #{}", request.number));
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
                let layer = fixture_stack(request.number)
                    .layers
                    .into_iter()
                    .find(|layer| layer.pull.number == request.number)
                    .expect("known pull request");
                let base_branch = request.base_branch.unwrap_or(layer.pull.base_branch);
                encode(&ReviewPullFetchResult {
                    number: request.number,
                    head_ref: format!("refs/heads/{}", layer.pull.head_branch),
                    head_oid: rev_parse(&self.repo, &layer.pull.head_branch),
                    base_ref: format!("refs/heads/{base_branch}"),
                    base_oid: rev_parse(&self.repo, &base_branch),
                    base_branch,
                })
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

impl ProviderTransport for FakeProvider {
    fn plugins_serving(
        &self,
        interface_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, ProviderCallError>> + Send {
        std::future::ready(Ok(if interface_id == REVIEW_SOURCE_INTERFACE_ID {
            self.plugins.clone()
        } else {
            Vec::new()
        }))
    }

    fn invoke(
        &self,
        plugin_id: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderCallError>> + Send {
        assert_eq!(plugin_id, FAKE_PLUGIN);
        assert_eq!(interface_id, REVIEW_SOURCE_INTERFACE_ID);
        std::future::ready(self.answer(operation, &payload))
    }
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
    transport: &FakeProvider,
    repo: &Path,
    sources: Vec<ReviewSource>,
) -> ResolveReviewSourcesResponse {
    let request = block_on(resolve_request_with_providers(transport, repo, sources))
        .expect("provider routing");
    // The request crosses the service boundary as JSON.
    let request: ResolveReviewSourcesRequest =
        serde_json::from_slice(&serde_json::to_vec(&request).expect("encode")).expect("decode");
    V2Service::default()
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
fn provider_backed_sources_report_provider_required_when_no_provider_is_enabled() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    // The provider plugin is disabled: nothing serves `bcode.review_source/v1`.
    let mut provider = FakeProvider::new(repo);
    provider.plugins.clear();
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

    // Workspaces without provider-backed sources never ask for providers.
    let request = block_on(resolve_request_with_providers(
        &enabled,
        repo,
        vec![source("last", ReviewSourceKind::LastCommit)],
    ))
    .expect("request");
    assert_eq!(request.provider_resolutions.len(), 0);
    assert_eq!(enabled.calls().len(), 1);
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
    let response = V2Service::default()
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
    let response = V2Service::default()
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
    service: &mut V2Service,
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
    let mut service = V2Service::default();
    let attribution = attribute(&mut service, repo, &stack, STACK_SHARED_FILE, None);
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
        &mut service,
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
    let untouched = attribute(
        &mut service,
        repo,
        &stack,
        &GitFixture::modified_path(0),
        None,
    );
    assert!(untouched.new_ranges.is_empty() && untouched.old_ranges.is_empty());
}

#[test]
fn attribution_is_served_as_a_code_review_operation() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let stack = whole_stack(repo);
    let request = ServiceRequest {
        interface_id: bcode_code_review_models::CODE_REVIEW_SERVICE_INTERFACE_ID.into(),
        operation: OP_REVIEW_STACK_ATTRIBUTION.into(),
        payload: serde_json::to_vec(&ReviewStackAttributionRequest {
            repo_path: repo.to_path_buf(),
            stack: stack.clone(),
            path: GitFixture::stack_layer_path(3),
            old_path: None,
        })
        .expect("encode"),
    };
    let response = V2Service::default()
        .invoke(&request)
        .expect("handled by the V2 service");
    assert!(response.error.is_none(), "{:?}", response.error);
    let attribution: ReviewStackAttributionResponse =
        serde_json::from_slice(&response.payload).expect("decode");
    assert_eq!(attribution.new_line_layer(1), Some(103));

    // Unconfined paths and malformed stacks are rejected.
    let mut bad = ReviewStackAttributionRequest {
        repo_path: repo.to_path_buf(),
        stack,
        path: "../outside".into(),
        old_path: None,
    };
    assert!(V2Service::default().stack_attribution(&bad).is_err());
    bad.path = STACK_SHARED_FILE.into();
    bad.stack.layers.clear();
    assert!(V2Service::default().stack_attribution(&bad).is_err());
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
            },
            ResolvedStackLayer {
                number: 2,
                title: "two".into(),
                base_oid: head_1,
                head_oid: head_2,
            },
        ],
    };
    let attribution = attribute(
        &mut V2Service::default(),
        &repo,
        &stack,
        "g.rs",
        Some("f.rs"),
    );
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
