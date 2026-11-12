//! Client-side discovery (`bcode.review_discovery/v1`): gathering contributors' local evidence
//! into hints for review-source providers and warnings on the stacks they resolve.
//!
//! Contributors only suggest; the provider decides. Hints change how many queries a provider
//! makes, never which stack it resolves. A contributor that fails or answers something
//! unreadable is skipped with a warning. See `docs/review-discovery.md`.

pub mod git;

use crate::provider::{
    ProviderCallError, ProviderRoute, ProviderTransport, call_interface, lookup_pulls,
    resolve_stack,
};
use bcode_code_review_models::{
    AnnotationSeverity, BranchAnnotation, BranchLink, OP_REVIEW_DISCOVERY_DISCOVER,
    OP_REVIEW_DISCOVERY_MANIFEST, REVIEW_DISCOVERY_INTERFACE_ID, REVIEW_DISCOVERY_MAX_ITEMS,
    ReviewDiscoveryManifest, ReviewDiscoveryRequest, ReviewDiscoveryResponse,
    ReviewSourceStackResolveResponse, ReviewStack,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Most hinted branches sent with one stack resolution.
pub const MAX_HINTS: usize = 400;

/// One enabled discovery contributor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// Plugin serving it.
    pub plugin_id: String,
    /// Its manifest.
    pub manifest: ReviewDiscoveryManifest,
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

/// The enabled discovery contributors. A contributor whose manifest cannot be read is skipped.
///
/// # Errors
///
/// * Returns the transport error when the plugin services cannot be listed.
pub async fn discover_contributors<T: ProviderTransport + Sync>(
    transport: &T,
) -> Result<Vec<Contributor>, ProviderCallError> {
    let mut contributors = Vec::new();
    for plugin_id in transport
        .plugins_serving(REVIEW_DISCOVERY_INTERFACE_ID)
        .await?
    {
        if let Ok(manifest) = call_interface::<_, _, ReviewDiscoveryManifest>(
            transport,
            &plugin_id,
            REVIEW_DISCOVERY_INTERFACE_ID,
            OP_REVIEW_DISCOVERY_MANIFEST,
            &serde_json::json!({}),
        )
        .await
        {
            contributors.push(Contributor {
                plugin_id,
                manifest,
            });
        }
    }
    Ok(contributors)
}

/// Ask every contributor about `branches` and merge their answers; failures are recorded, not
/// returned.
pub async fn discover<T: ProviderTransport + Sync>(
    transport: &T,
    contributors: &[Contributor],
    repo_path: &Path,
    branches: &[String],
    trunk: Option<&str>,
) -> Discovery {
    let request = ReviewDiscoveryRequest {
        repo_path: repo_path.to_path_buf(),
        branches: branches.to_vec(),
        trunk: trunk.map(str::to_owned),
    };
    let mut discovery = Discovery::default();
    for contributor in contributors {
        let label = contributor.manifest.label.as_str();
        match call_interface::<_, _, ReviewDiscoveryResponse>(
            transport,
            &contributor.plugin_id,
            REVIEW_DISCOVERY_INTERFACE_ID,
            OP_REVIEW_DISCOVERY_DISCOVER,
            &request,
        )
        .await
        {
            Ok(response) => discovery.add(label, response),
            Err(error) => discovery.failures.push((label.to_owned(), error.message)),
        }
    }
    discovery
}

/// Resolve the stack containing pull request `number` through `route` with discovery.
///
/// The contributors are asked about its head branch (`head_branch`, else looked up by number when
/// any contributor is enabled); their hints go with the resolution and their warnings onto the
/// stack. Without contributors this is [`resolve_stack`].
///
/// # Errors
///
/// * The provider's error; discovery failures become warnings instead.
pub async fn resolve_stack_with_discovery<T: ProviderTransport + Sync>(
    transport: &T,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    number: u64,
    max_layers: Option<usize>,
    head_branch: Option<String>,
) -> Result<ReviewSourceStackResolveResponse, ProviderCallError> {
    let contributors = discover_contributors(transport).await.unwrap_or_default();
    let mut discovery = Discovery::default();
    let mut hints = Vec::new();
    if !contributors.is_empty() {
        let head = match head_branch {
            Some(head) => Some(head),
            None => lookup_pulls(
                transport,
                route,
                repo_path,
                repository.clone(),
                &number.to_string(),
            )
            .await
            .ok()
            .and_then(|listed| listed.pulls.into_iter().find(|pull| pull.number == number))
            .map(|pull| pull.head_branch),
        };
        if let Some(head) = head {
            discovery = discover(
                transport,
                &contributors,
                repo_path,
                std::slice::from_ref(&head),
                None,
            )
            .await;
            hints = discovery.hints(&head);
        }
    }
    let mut response = resolve_stack(
        transport, route, repo_path, repository, number, max_layers, hints,
    )
    .await?;
    discovery.annotate(&mut response.stack);
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
