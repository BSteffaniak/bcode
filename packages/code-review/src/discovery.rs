//! Client-side discovery (`bcode.review_discovery/v1`): gathering contributors' local evidence
//! into hints for review-source providers and warnings on the stacks they resolve.
//!
//! Contributors only suggest; the provider decides. Hints change how many queries a provider
//! makes, never which stack it resolves. A contributor that fails or answers something
//! unreadable is skipped with a warning. See `docs/review-discovery.md`.

pub mod git;

use crate::provider::{
    Freshness, ProviderCallError, ProviderRegistry, ProviderRoute, lookup_pulls, remembered_stack,
    resolve_stack,
};
use bcode_code_review_models::{
    AnnotationSeverity, BranchAnnotation, BranchLink, REVIEW_DISCOVERY_MAX_ITEMS,
    ReviewDiscoveryRequest, ReviewDiscoveryResponse, ReviewSourceStackResolveResponse, ReviewStack,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Most hinted branches sent with one stack resolution.
pub const MAX_HINTS: usize = 400;

/// One branch of a stack as discovery sees it, before a provider confirms it: shown as
/// unconfirmed, never used for anything that acts on the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalLayer {
    /// The branch.
    pub branch: String,
    /// The branch it is stacked on.
    pub parent: String,
}

/// What every contributor said, each item with the label of the contributor that said it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Parent links.
    pub links: Vec<(String, BranchLink)>,
    /// Related branches, from every contributor.
    pub related: BTreeSet<String>,
    /// Annotations.
    pub annotations: Vec<(String, BranchAnnotation)>,
    /// Contributors that could not answer: label and reason.
    pub failures: Vec<(String, String)>,
}

impl Discovery {
    /// Add `contributor`'s answer, keeping at most [`REVIEW_DISCOVERY_MAX_ITEMS`] of each kind.
    pub fn add(&mut self, label: &str, response: ReviewDiscoveryResponse) {
        self.links.extend(
            response
                .links
                .into_iter()
                .take(REVIEW_DISCOVERY_MAX_ITEMS)
                .map(|link| (label.to_owned(), link)),
        );
        self.related.extend(
            response
                .related
                .into_iter()
                .take(REVIEW_DISCOVERY_MAX_ITEMS),
        );
        self.annotations.extend(
            response
                .annotations
                .into_iter()
                .take(REVIEW_DISCOVERY_MAX_ITEMS)
                .map(|annotation| (label.to_owned(), annotation)),
        );
    }

    /// Branches to look up together when resolving the stack of `seed`: the branches the links
    /// connect to it (in either direction), then the related ones, at most [`MAX_HINTS`]. Links
    /// are followed only through branches that have a parent link themselves: the trunk, which
    /// has none, is where stacks meet, not a path between them.
    #[must_use]
    pub fn hints(&self, seed: &str) -> Vec<String> {
        let mut neighbors: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let children: BTreeSet<&str> = self
            .links
            .iter()
            .map(|(_, link)| link.branch.as_str())
            .collect();
        for (_, link) in &self.links {
            neighbors
                .entry(link.branch.as_str())
                .or_default()
                .push(link.parent.as_str());
            neighbors
                .entry(link.parent.as_str())
                .or_default()
                .push(link.branch.as_str());
        }
        let mut visited: BTreeSet<&str> = BTreeSet::from([seed]);
        let mut queue = vec![seed];
        let mut hints = Vec::new();
        while let Some(branch) = queue.pop() {
            if branch != seed && !children.contains(branch) {
                continue;
            }
            for next in neighbors.get(branch).into_iter().flatten() {
                if visited.insert(next) {
                    hints.push((*next).to_owned());
                    queue.push(next);
                }
            }
        }
        for branch in &self.related {
            if visited.insert(branch) {
                hints.push(branch.clone());
            }
        }
        hints.truncate(MAX_HINTS);
        hints
    }

    /// The chain through `seed` as the links describe it, bottom first: down through each
    /// branch's parent until a branch without one (the trunk), then up while exactly one branch
    /// is stacked on the current one. Contributors that disagree about a branch's parent end the
    /// chain there. Empty when no link names `seed`; at most [`MAX_HINTS`] layers.
    #[must_use]
    pub fn provisional_chain(&self, seed: &str) -> Vec<ProvisionalLayer> {
        let mut parents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (_, link) in &self.links {
            parents
                .entry(link.branch.as_str())
                .or_default()
                .insert(link.parent.as_str());
            children
                .entry(link.parent.as_str())
                .or_default()
                .insert(link.branch.as_str());
        }
        let only = |set: Option<&BTreeSet<&'_ str>>| -> Option<String> {
            let set = set?;
            (set.len() == 1).then(|| set.iter().next().map(|item| (*item).to_owned()))?
        };
        let mut visited: BTreeSet<String> = BTreeSet::from([seed.to_owned()]);
        let mut below = Vec::new();
        let mut current = seed.to_owned();
        while let Some(parent) = only(parents.get(current.as_str())) {
            below.push(ProvisionalLayer {
                branch: current.clone(),
                parent: parent.clone(),
            });
            if !visited.insert(parent.clone()) || below.len() >= MAX_HINTS {
                break;
            }
            current = parent;
        }
        let mut chain: Vec<ProvisionalLayer> = below.into_iter().rev().collect();
        // A seed without a parent of its own is not part of a chain.
        if chain.is_empty() {
            return chain;
        }
        let mut current = seed.to_owned();
        while chain.len() < MAX_HINTS
            && let Some(child) = only(children.get(current.as_str()))
            && visited.insert(child.clone())
        {
            chain.push(ProvisionalLayer {
                branch: child.clone(),
                parent: current,
            });
            current = child;
        }
        chain
    }

    /// Add the warnings that concern `stack` to it: warning annotations on its layers' branches,
    /// links that disagree with a layer's actual base, and contributors that failed.
    pub fn annotate(&self, stack: &mut ReviewStack) {
        for layer in &stack.layers {
            let (number, head, base) = (
                layer.pull.number,
                layer.pull.head_branch.as_str(),
                layer.pull.base_branch.as_str(),
            );
            for (label, annotation) in &self.annotations {
                if annotation.branch == head && annotation.severity == AnnotationSeverity::Warning {
                    stack
                        .warnings
                        .push(format!("{label}: #{number} {}", annotation.message));
                }
            }
            for (label, link) in &self.links {
                if link.branch == head && link.parent != base {
                    stack.warnings.push(format!(
                        "{label} records {} as the parent of #{number}, but its pull request's \
                         base is {base}",
                        link.parent
                    ));
                }
            }
        }
        for (label, reason) in &self.failures {
            stack
                .warnings
                .push(format!("{label} could not be asked: {reason}"));
        }
    }
}

/// Ask every contributor of `registry` about `branches`, in parallel, and merge their answers;
/// failures are recorded, not returned.
#[must_use]
pub fn discover(
    registry: &ProviderRegistry,
    repo_path: &Path,
    branches: &[String],
    trunk: Option<&str>,
) -> Discovery {
    let request = ReviewDiscoveryRequest {
        repo_path: repo_path.to_path_buf(),
        branches: branches.to_vec(),
        trunk: trunk.map(str::to_owned),
    };
    let answers: Vec<(String, Result<ReviewDiscoveryResponse, ProviderCallError>)> =
        std::thread::scope(|scope| {
            // Every contributor starts before any is joined, so they run at once.
            let mut workers = Vec::new();
            for route in registry.contributors() {
                let request = &request;
                workers.push(scope.spawn(move || {
                    (
                        route.manifest.label.clone(),
                        route.contributor.discover(request),
                    )
                }));
            }
            workers
                .into_iter()
                .filter_map(|worker| worker.join().ok())
                .collect()
        });
    let mut discovery = Discovery::default();
    for (label, answer) in answers {
        match answer {
            Ok(response) => discovery.add(&label, response),
            Err(error) => discovery.failures.push((label, error.message)),
        }
    }
    discovery
}

/// The stack through branch `head` as `registry`'s contributors see it (see
/// [`Discovery::provisional_chain`]): local and fast, to show before a provider confirms it.
#[must_use]
pub fn provisional_stack(
    registry: &ProviderRegistry,
    repo_path: &Path,
    head: &str,
) -> Vec<ProvisionalLayer> {
    if registry.contributors().is_empty() {
        return Vec::new();
    }
    discover(registry, repo_path, &[head.to_owned()], None).provisional_chain(head)
}

/// Resolve the stack containing pull request `number` through `route` with discovery.
///
/// A stack `registry` remembers with `number` as a layer is returned, confirmed with the provider
/// first when it is not fresh (see [`remembered_stack`]). Otherwise the contributors are asked about its head
/// branch (`head_branch`, else looked up by number when any contributor is registered); their
/// hints go with the resolution and their warnings onto the stack, which is remembered. Without
/// contributors this is [`resolve_stack`].
///
/// # Errors
///
/// * The provider's error; discovery failures become warnings instead.
pub fn resolve_stack_with_discovery(
    registry: &ProviderRegistry,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    number: u64,
    max_layers: Option<usize>,
    head_branch: Option<String>,
) -> Result<ReviewSourceStackResolveResponse, ProviderCallError> {
    // A stack resolved before (by the home screen, the wizard, or another open) is reused,
    // warnings included, once confirmed.
    if let Some(repository) = repository.as_deref()
        && let Some(stack) = remembered_stack(
            registry,
            route,
            repo_path,
            repository,
            number,
            Freshness::Confirmed,
        )
    {
        return Ok(ReviewSourceStackResolveResponse {
            repository: repository.to_owned(),
            stack,
        });
    }
    resolve_new_stack_with_discovery(
        registry,
        route,
        repo_path,
        repository,
        number,
        max_layers,
        head_branch,
    )
}

/// [`resolve_stack_with_discovery`] without looking for a remembered stack: the provider resolves
/// it (with the contributors' hints), and the answer is remembered.
///
/// # Errors
///
/// * The provider's error; discovery failures become warnings instead.
pub fn resolve_new_stack_with_discovery(
    registry: &ProviderRegistry,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    number: u64,
    max_layers: Option<usize>,
    head_branch: Option<String>,
) -> Result<ReviewSourceStackResolveResponse, ProviderCallError> {
    let mut discovery = Discovery::default();
    let mut hints = Vec::new();
    if !registry.contributors().is_empty() {
        // The head branch: given, else as a provider reported it before, else looked up.
        let noted = repository
            .as_deref()
            .and_then(|repository| registry.known_pull(&route.manifest.id, repository, number))
            .map(|pull| pull.head_branch);
        let head = head_branch.or(noted).or_else(|| {
            lookup_pulls(
                registry,
                route,
                repo_path,
                repository.clone(),
                &number.to_string(),
            )
            .ok()
            .and_then(|listed| listed.pulls.into_iter().find(|pull| pull.number == number))
            .map(|pull| pull.head_branch)
        });
        if let Some(head) = head {
            discovery = discover(registry, repo_path, std::slice::from_ref(&head), None);
            hints = discovery.hints(&head);
        }
    }
    let mut response = resolve_stack(route, repo_path, repository, number, max_layers, hints)?;
    discovery.annotate(&mut response.stack);
    registry.remember_stack(&route.manifest.id, &response.repository, &response.stack);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{PullRequestSummary, ReviewStackLayer};

    fn link(branch: &str, parent: &str) -> BranchLink {
        BranchLink {
            branch: branch.into(),
            parent: parent.into(),
            parent_oid: None,
        }
    }

    fn layer(number: u64, base: &str, head: &str) -> ReviewStackLayer {
        ReviewStackLayer {
            pull: PullRequestSummary {
                number,
                title: String::new(),
                author: None,
                base_branch: base.into(),
                head_branch: head.into(),
                head_oid: String::new(),
                base_oid: None,
                review_requested_from_me: false,
                draft: false,
                updated: None,
                url: None,
            },
            parent: None,
        }
    }

    #[test]
    fn hints_follow_links_both_ways_then_related_branches() {
        let mut discovery = Discovery::default();
        discovery.add(
            "Tool",
            ReviewDiscoveryResponse {
                links: vec![
                    link("a", "main"),
                    link("b", "a"),
                    link("c", "b"),
                    link("x", "y"),
                    // Another stack on the trunk is not part of b's.
                    link("other", "main"),
                ],
                related: vec!["d".into(), "b".into()],
                annotations: Vec::new(),
            },
        );
        let hints = discovery.hints("b");
        let set: BTreeSet<&str> = hints.iter().map(String::as_str).collect();
        // The seed's component (not the unrelated x → y), then related branches, once each.
        assert_eq!(set, BTreeSet::from(["a", "main", "c", "d"]));
        assert_eq!(hints.len(), set.len());
        assert_eq!(hints.last().map(String::as_str), Some("d"));
    }

    fn chain_of(discovery: &Discovery, seed: &str) -> Vec<(String, String)> {
        discovery
            .provisional_chain(seed)
            .into_iter()
            .map(|layer| (layer.branch, layer.parent))
            .collect()
    }

    #[test]
    fn the_provisional_chain_runs_from_the_trunk_through_the_seed_while_it_is_linear() {
        let mut discovery = Discovery::default();
        discovery.add(
            "Tool",
            ReviewDiscoveryResponse {
                links: vec![
                    link("a", "main"),
                    link("b", "a"),
                    link("c", "b"),
                    link("d", "c"),
                    // c has two branches on it: the chain stops at c going up from below.
                    link("e", "c"),
                    link("other", "main"),
                ],
                related: Vec::new(),
                annotations: Vec::new(),
            },
        );
        let chain = |seed: &str| chain_of(&discovery, seed);
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|(branch, parent)| ((*branch).to_owned(), (*parent).to_owned()))
                .collect()
        };
        assert_eq!(chain("b"), pairs(&[("a", "main"), ("b", "a"), ("c", "b")]));
        // From the top: everything below it.
        assert_eq!(
            chain("d"),
            pairs(&[("a", "main"), ("b", "a"), ("c", "b"), ("d", "c")])
        );
        // The trunk and unknown branches are not chains.
        assert_eq!(chain("main"), Vec::new());
        assert_eq!(chain("unknown"), Vec::new());
        // Contributors that disagree about a parent end the chain below that branch.
        discovery.add(
            "Other",
            ReviewDiscoveryResponse {
                links: vec![link("b", "x")],
                related: Vec::new(),
                annotations: Vec::new(),
            },
        );
        assert_eq!(chain_of(&discovery, "c"), pairs(&[("c", "b")]));
    }

    #[test]
    fn warnings_cover_annotations_disagreeing_links_and_failures() {
        let mut discovery = Discovery::default();
        discovery.add(
            "Tool",
            ReviewDiscoveryResponse {
                links: vec![link("b", "a"), link("c", "a")],
                related: Vec::new(),
                annotations: vec![
                    BranchAnnotation {
                        branch: "b".into(),
                        severity: AnnotationSeverity::Warning,
                        code: "needs_restack".into(),
                        message: "needs a restack".into(),
                    },
                    BranchAnnotation {
                        branch: "b".into(),
                        severity: AnnotationSeverity::Info,
                        code: "note".into(),
                        message: "is fine".into(),
                    },
                    BranchAnnotation {
                        branch: "elsewhere".into(),
                        severity: AnnotationSeverity::Warning,
                        code: "needs_restack".into(),
                        message: "needs a restack".into(),
                    },
                ],
            },
        );
        discovery
            .failures
            .push(("Other".into(), "timed out".into()));
        let mut stack = ReviewStack {
            trunk: "main".into(),
            rooted_at_default_branch: true,
            layers: vec![
                layer(1, "main", "a"),
                layer(2, "a", "b"),
                layer(3, "b", "c"),
            ],
            requested: 2,
            siblings: Vec::new(),
            warnings: Vec::new(),
        };
        discovery.annotate(&mut stack);
        assert_eq!(
            stack.warnings,
            [
                "Tool: #2 needs a restack",
                "Tool records a as the parent of #3, but its pull request's base is b",
                "Other could not be asked: timed out",
            ]
        );
    }
}
